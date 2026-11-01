//! How a plugin editor window's title bar looks: the colour of the track the plugin is open on,
//! softened toward the dark window body. Kept in `plugin-window.json` in the data folder's
//! `Settings` folder (`Documents/Soundcraft/Settings`), next to the plugin scanner's preferences.

use std::path::{Path, PathBuf};

/// The file in the `Settings` folder.
pub const FILE: &str = "plugin-window.json";

/// The dark the track colour is softened toward.
const DARK: u32 = 0x20;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EditorFrameSettings {
    /// The title bar takes the track's colour (otherwise a fixed slate).
    pub track_color: bool,
    /// How much of the track colour shows, in percent: 100 is the colour itself, 0 the dark body.
    pub strength: u8,
    #[serde(skip)]
    dir: Option<PathBuf>,
}

impl Default for EditorFrameSettings {
    fn default() -> Self {
        Self { track_color: true, strength: 55, dir: None }
    }
}

impl EditorFrameSettings {
    /// The settings saved in `dir` (defaults when missing or unreadable); later [`save`](Self::save)s go there.
    pub fn load(dir: &Path) -> Self {
        let read = std::fs::read(dir.join(FILE)).ok().and_then(|b| serde_json::from_slice::<Self>(&b).ok());
        Self { dir: Some(dir.to_path_buf()), ..read.unwrap_or_default() }
    }

    /// Writes the settings to the folder they were loaded from; a no-op without one.
    pub fn save(&self) -> Result<(), String> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let json = serde_json::to_vec_pretty(self).map_err(|e| format!("cannot save the plugin window settings: {e}"))?;
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(dir.join(FILE), json))
            .map_err(|e| format!("cannot save the plugin window settings: {e}"))
    }

    /// The title bar colour for a plugin on a track of colour `track`; `None` for the default.
    pub fn bar_color(&self, track: [u8; 3]) -> Option<[u8; 3]> {
        let s = u32::from(self.strength.min(100));
        self.track_color.then(|| track.map(|v| ((u32::from(v) * s + DARK * (100 - s)) / 100) as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_blends_toward_the_dark_body() {
        let s = |strength| EditorFrameSettings { strength, ..Default::default() };
        assert_eq!(s(100).bar_color([200, 60, 60]), Some([200, 60, 60]));
        assert_eq!(s(0).bar_color([200, 60, 60]), Some([0x20; 3]));
        assert_eq!(s(50).bar_color([200, 60, 60]), Some([116, 46, 46]));
        assert_eq!(EditorFrameSettings { track_color: false, ..Default::default() }.bar_color([1, 2, 3]), None);
    }

    #[test]
    fn saves_and_loads_in_the_folder() {
        let dir = std::env::temp_dir().join(format!("sc-editor-frame-{}", std::process::id()));
        let mut s = EditorFrameSettings::load(&dir);
        assert_eq!(s, EditorFrameSettings { dir: Some(dir.clone()), ..Default::default() });
        s.strength = 30;
        s.track_color = false;
        assert!(s.save().is_ok());
        assert_eq!(EditorFrameSettings::load(&dir), s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
