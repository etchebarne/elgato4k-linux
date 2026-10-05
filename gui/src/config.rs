//! Tiny `key=value` preferences file in `$XDG_CONFIG_HOME/elgato4k-gui/`.

use std::fs;
use std::path::PathBuf;

use gtk::glib;

#[derive(Clone, Debug)]
pub struct Config {
    /// Label of the last chosen video mode (see `VideoMode::label`).
    pub video_mode: Option<String>,
    pub hardware_decode: bool,
    /// Slider position, 0.0–1.0 (mapped to a cubic gain curve).
    pub volume: f64,
    pub muted: bool,
    pub show_sidebar: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { video_mode: None, hardware_decode: false, volume: 0.8, muted: false, show_sidebar: true }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("elgato4k-gui").join("settings")
}

impl Config {
    pub fn load() -> Self {
        let mut config = Self::default();
        let Ok(text) = fs::read_to_string(path()) else { return config };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "video_mode" if !value.is_empty() => config.video_mode = Some(value.to_string()),
                "hardware_decode" => config.hardware_decode = value == "true",
                "volume" => config.volume = value.parse().unwrap_or(config.volume).clamp(0.0, 1.0),
                "muted" => config.muted = value == "true",
                "show_sidebar" => config.show_sidebar = value == "true",
                _ => {}
            }
        }
        config
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let text = format!(
            "video_mode={}\nhardware_decode={}\nvolume={:.3}\nmuted={}\nshow_sidebar={}\n",
            self.video_mode.as_deref().unwrap_or(""),
            self.hardware_decode,
            self.volume,
            self.muted,
            self.show_sidebar,
        );
        if let Err(e) = fs::write(&path, text) {
            eprintln!("failed to save {}: {e}", path.display());
        }
    }
}
