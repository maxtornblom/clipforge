use std::{fs, path::PathBuf};

use gtk::glib;
use serde::{Deserialize, Serialize};

/// Persistent settings. Same file and `clips_dir` key as the Python version,
/// so an existing config keeps working; new fields default when absent.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub clips_dir: String,
    pub favorites: Vec<String>,
    pub exact_export: bool,
    /// Save a clip automatically when you get a kill in Counter-Strike 2.
    pub autoclip_cs2: bool,
    /// Seconds of footage to keep after the last kill of a streak.
    pub clip_delay: u32,
    /// Length of an auto-clip in seconds (the saved buffer is trimmed to this).
    pub clip_length: u32,
    /// Let ClipForge run the GPU Screen Recorder replay buffer itself.
    pub managed_replay: bool,
    /// Replay buffer length when ClipForge runs it.
    pub replay_secs: u32,
    /// Open 4:3 clips in the editor with "Fill screen" already selected.
    pub fill_43_default: bool,
    /// Stretch freshly saved 4:3 clips to fullscreen 16:9 (originals go to an Originals folder).
    pub autoclip_fill: bool,
    /// Flash a border around the screen when a clip is saved.
    pub flash_border: bool,
}

impl Default for Config {
    fn default() -> Self {
        let dir = std::env::var("GSR_CLIPS_DIR")
            .unwrap_or_else(|_| glib::home_dir().join("Videos").to_string_lossy().into_owned());
        Self {
            clips_dir: dir,
            favorites: Vec::new(),
            exact_export: false,
            autoclip_cs2: false,
            clip_delay: 5,
            clip_length: 30,
            managed_replay: false,
            replay_secs: 60,
            fill_43_default: true,
            autoclip_fill: true,
            flash_border: true,
        }
    }
}

fn config_path() -> PathBuf {
    glib::user_config_dir().join("clipforge").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    pub fn is_favorite(&self, path: &str) -> bool {
        self.favorites.iter().any(|f| f == path)
    }

    pub fn toggle_favorite(&mut self, path: &str) -> bool {
        if let Some(i) = self.favorites.iter().position(|f| f == path) {
            self.favorites.remove(i);
            false
        } else {
            self.favorites.push(path.to_owned());
            true
        }
    }

    pub fn rename_favorite(&mut self, from: &str, to: &str) {
        for f in self.favorites.iter_mut().filter(|f| *f == from) {
            *f = to.to_owned();
        }
    }

    pub fn remove_favorite(&mut self, path: &str) {
        self.favorites.retain(|f| f != path);
    }
}
