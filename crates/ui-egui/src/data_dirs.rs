//! The SoundCraft data folder: one root (default `Documents/Soundcraft`) holding every user-visible
//! file the app keeps, in named subfolders:
//!
//! | folder      | holds                                                            |
//! |-------------|------------------------------------------------------------------|
//! | `Projects`  | where new projects are saved                                     |
//! | `Presets`   | plugin presets, one folder per plugin named exactly like the plugin |
//! | `Templates` | saved templates (`.scraft`), listed by File > New > Open from Template |
//! | `Settings`  | plugin scanner preferences (`plugin-scan.json`)                  |
//! | `Autosave`  | autosaved copies of unsaved work                                 |
//! | `Cache`     | the saved plugin scan (`plugin-cache.json`)                      |
//! | `Logs`      | `soundcraft.log` and its rotations                               |
//!
//! `ui.json` stays in the per-user settings folder (`%APPDATA%/SoundCraft`): it holds the root
//! itself, so it must be readable before the root is known. Nothing here is ever deleted: moving the
//! root and migrating from the old settings folder only copy files that are missing at the target.

use std::io;
use std::path::{Path, PathBuf};

pub const PROJECTS: &str = "Projects";
pub const PRESETS: &str = "Presets";
pub const TEMPLATES: &str = "Templates";
pub const SETTINGS: &str = "Settings";
pub const AUTOSAVE: &str = "Autosave";
pub const CACHE: &str = "Cache";
pub const LOGS: &str = "Logs";

/// Written in the root once the old settings folder was copied, so a later start does not bring back
/// autosaves that were recovered or discarded since.
const MIGRATED_MARKER: &str = ".migrated-from-settings";

/// The folders under one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    pub root: PathBuf,
    pub projects: PathBuf,
    pub presets: PathBuf,
    pub templates: PathBuf,
    pub settings: PathBuf,
    pub autosave: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}

/// `Documents/Soundcraft` in the user's home folder (empty in the browser, which has no folders).
pub fn default_root() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| ".".into());
        Path::new(&home).join("Documents").join("Soundcraft")
    }
    #[cfg(target_arch = "wasm32")]
    {
        PathBuf::new()
    }
}

impl DataDirs {
    pub fn new(root: impl Into<PathBuf>) -> DataDirs {
        let root = root.into();
        DataDirs {
            projects: root.join(PROJECTS),
            presets: root.join(PRESETS),
            templates: root.join(TEMPLATES),
            settings: root.join(SETTINGS),
            autosave: root.join(AUTOSAVE),
            cache: root.join(CACHE),
            logs: root.join(LOGS),
            root,
        }
    }

    /// The root a preference names; blank means [`default_root`].
    pub fn from_pref(root: &str) -> DataDirs {
        let r = root.trim();
        DataDirs::new(if r.is_empty() { default_root() } else { PathBuf::from(r) })
    }

    fn all(&self) -> [&Path; 7] {
        [&self.projects, &self.presets, &self.templates, &self.settings, &self.autosave, &self.cache, &self.logs]
    }

    /// Creates the root and every subfolder (existing ones are left alone).
    pub fn ensure(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        for d in self.all() {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// Copies what the old per-user settings folder `old` holds into this layout, once (a marker file
    /// stops later starts). Originals stay; files already present at the target are kept.
    pub fn migrate_from(&self, old: &Path) -> io::Result<()> {
        if old == self.root || self.root.join(MIGRATED_MARKER).exists() {
            return Ok(());
        }
        copy_missing(&old.join("Presets"), &self.presets)?;
        copy_missing(&old.join("Autosave"), &self.autosave)?;
        copy_missing(&old.join("logs"), &self.logs)?;
        copy_missing_file(&old.join("plugin-cache.json"), &self.cache.join("plugin-cache.json"))?;
        copy_missing_file(&old.join("plugin-scan.json"), &self.settings.join("plugin-scan.json"))?;
        std::fs::write(self.root.join(MIGRATED_MARKER), b"")
    }

    /// Copies the small user content (presets, templates, settings, cache) of another root into this
    /// one when the root is changed. Projects, autosaves and logs stay where they are.
    pub fn copy_from_root(&self, old: &DataDirs) -> io::Result<()> {
        if old.root == self.root {
            return Ok(());
        }
        copy_missing(&old.presets, &self.presets)?;
        copy_missing(&old.templates, &self.templates)?;
        copy_missing(&old.settings, &self.settings)?;
        copy_missing(&old.cache, &self.cache)
    }
}

/// A template found in the Templates folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub name: String,
    pub path: PathBuf,
}

/// The templates in `dir`, sorted by name ignoring case: `<name>.scraft` files, and `<name>/<name>.scraft`
/// project folders. A missing or unreadable folder lists nothing.
pub fn list_templates(dir: &Path) -> Vec<Template> {
    let ext = soundcraft_model::SESSION_EXTENSION;
    let mut found = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return found };
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str).map(str::to_string) else { continue };
        let is_template = path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
        if is_template {
            found.push(Template { name: stem, path });
        } else if path.is_dir() {
            let inner = path.join(format!("{}.{ext}", entry.file_name().to_string_lossy()));
            if inner.is_file() {
                found.push(Template { name: entry.file_name().to_string_lossy().into_owned(), path: inner });
            }
        }
    }
    found.sort_by_key(|t| (t.name.to_lowercase(), t.name.clone()));
    found
}

/// A plugin's preset folder: `<presets>/<exact plugin name>`. Only characters a file name cannot
/// hold are replaced.
pub fn preset_folder(presets: &Path, plugin_name: &str) -> PathBuf {
    presets.join(preset_folder_name(plugin_name))
}

/// The plugin's name as a folder name: unchanged except `\ / : * ? " < > |` and control characters.
pub fn preset_folder_name(plugin_name: &str) -> String {
    let clean: String = plugin_name.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() { "_".into() } else { clean }
}

/// Copies the file `from` to `to` unless `to` exists (or `from` does not).
fn copy_missing_file(from: &Path, to: &Path) -> io::Result<()> {
    if !from.is_file() || to.exists() {
        return Ok(());
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::copy(from, to).map(|_| ())
}

/// Copies the tree `from` into `to`, skipping every file that already exists there. Never deletes.
pub fn copy_missing(from: &Path, to: &Path) -> io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        let path = entry.path();
        if path.is_dir() {
            copy_missing(&path, &target)?;
        } else {
            copy_missing_file(&path, &target)?;
        }
    }
    Ok(())
}

impl crate::SoundApp {
    /// Moves the data root to `new_root`: creates the subfolders there, copies presets, templates and
    /// settings over from the old root (nothing is deleted), and points presets, templates and the
    /// default projects folder at it. Autosave, cache and logs switch at the next start.
    pub fn change_data_root(&mut self, new_root: &str) -> Result<String, String> {
        let new = DataDirs::from_pref(new_root);
        let old = self.data_dirs.clone().unwrap_or_else(|| DataDirs::from_pref(&self.ui.preferences.data_root));
        new.ensure().map_err(|e| format!("Could not create {}: {e}", new.root.display()))?;
        new.copy_from_root(&old).map_err(|e| format!("Could not copy to {}: {e}", new.root.display()))?;
        let prefs = &mut self.ui.preferences;
        if prefs.default_projects_dir.trim().is_empty() || Path::new(prefs.default_projects_dir.trim()) == old.projects {
            prefs.default_projects_dir = new.projects.to_string_lossy().into_owned();
        }
        prefs.data_root = new.root.to_string_lossy().into_owned();
        self.preset_dir = Some(new.presets.clone());
        self.template_dir = Some(new.templates.clone());
        let msg = format!("Data folder: {} (autosave, cache and logs move at the next start)", new.root.display());
        self.data_dirs = Some(new);
        self.recent_changed = true;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sc-datadirs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn default_paths_end_with_soundcraft_folders() {
        let root = default_root();
        assert!(root.ends_with("Documents/Soundcraft") || root.ends_with("Documents\\Soundcraft"), "{root:?}");
        let d = DataDirs::new(&root);
        assert!(d.projects.ends_with("Soundcraft/Projects"));
        assert!(d.presets.ends_with("Soundcraft/Presets"));
        assert!(d.templates.ends_with("Soundcraft/Templates"));
        assert_eq!(DataDirs::from_pref("  ").root, root);
    }

    #[test]
    fn ensure_creates_the_subfolders() {
        let root = tmp("ensure").join("Soundcraft");
        let d = DataDirs::new(&root);
        d.ensure().expect("ensure");
        for n in [PROJECTS, PRESETS, TEMPLATES, SETTINGS, AUTOSAVE, CACHE, LOGS] {
            assert!(root.join(n).is_dir(), "{n}");
        }
    }

    #[test]
    fn preset_folder_is_the_exact_plugin_name() {
        let p = Path::new("/r/Presets");
        assert_eq!(preset_folder(p, "FabFilter Pro-Q 3"), p.join("FabFilter Pro-Q 3"));
        assert_eq!(preset_folder(p, "7-Band EQ"), p.join("7-Band EQ"));
        assert_eq!(preset_folder_name("A/B: C?"), "A_B_ C_");
        assert_eq!(preset_folder_name(""), "_");
    }

    #[test]
    fn templates_are_listed_sorted_without_extension() {
        let dir = tmp("templates");
        for f in ["beta.scraft", "Alpha.scraft", "gamma.scraft", "notes.txt"] {
            std::fs::write(dir.join(f), b"{}").expect("write");
        }
        std::fs::create_dir_all(dir.join("Delta")).expect("dir");
        std::fs::write(dir.join("Delta").join("Delta.scraft"), b"{}").expect("write");
        let names: Vec<String> = list_templates(&dir).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["Alpha", "beta", "Delta", "gamma"]);
        assert!(list_templates(&dir.join("missing")).is_empty());
    }

    #[test]
    fn migration_copies_keeps_existing_and_never_deletes() {
        let base = tmp("migrate");
        let old = base.join("appdata");
        std::fs::create_dir_all(old.join("Presets/eq")).expect("dir");
        std::fs::write(old.join("Presets/eq/a.json"), b"old").expect("write");
        std::fs::write(old.join("ui.json"), b"{}").expect("write");
        let d = DataDirs::new(base.join("Soundcraft"));
        d.ensure().expect("ensure");
        std::fs::create_dir_all(d.presets.join("eq")).expect("dir");
        std::fs::write(d.presets.join("eq/a.json"), b"new").expect("write");
        std::fs::write(old.join("Presets/eq/b.json"), b"b").expect("write");
        d.migrate_from(&old).expect("migrate");
        assert_eq!(std::fs::read(d.presets.join("eq/a.json")).expect("a"), b"new");
        assert_eq!(std::fs::read(d.presets.join("eq/b.json")).expect("b"), b"b");
        assert!(old.join("Presets/eq/a.json").is_file() && old.join("Presets/eq/b.json").is_file() && old.join("ui.json").is_file());
        // Idempotent: a second run changes nothing.
        d.migrate_from(&old).expect("again");
        assert_eq!(std::fs::read(d.presets.join("eq/a.json")).expect("a"), b"new");
    }

    #[test]
    fn changing_the_root_creates_folders_and_keeps_old_files() {
        let base = tmp("change");
        let old = DataDirs::new(base.join("Old"));
        old.ensure().expect("ensure");
        std::fs::write(old.templates.join("t.scraft"), b"{}").expect("write");
        let mut app = crate::SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
        app.data_dirs = Some(old.clone());
        app.ui.preferences.default_projects_dir = old.projects.to_string_lossy().into_owned();
        let new_root = base.join("New");
        app.change_data_root(&new_root.to_string_lossy()).expect("change");
        let new = DataDirs::new(&new_root);
        for d in new.all() {
            assert!(d.is_dir(), "{d:?}");
        }
        assert!(new.templates.join("t.scraft").is_file());
        assert!(old.templates.join("t.scraft").is_file());
        assert_eq!(app.template_dir.as_deref(), Some(new.templates.as_path()));
        assert_eq!(app.ui.preferences.default_projects_dir, new.projects.to_string_lossy());
    }
}
