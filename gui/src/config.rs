//! Tiny `key=value` preferences file in `$XDG_CONFIG_HOME/elgato4k-gui/`.

use std::fs;
use std::path::PathBuf;

use gtk::glib;

use crate::framegen;

#[derive(Clone, Debug)]
pub struct Config {
    /// Label of the last chosen video mode (see `VideoMode::label`).
    pub video_mode: Option<String>,
    pub hardware_decode: bool,
    /// Slider position, 0.0–1.0 (mapped to a cubic gain curve).
    pub volume: f64,
    pub muted: bool,
    /// Sink name of the chosen output device; `None` follows the system default.
    pub audio_output: Option<String>,
    pub show_sidebar: bool,
    pub frame_gen: framegen::Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self { video_mode: None, hardware_decode: false, volume: 0.8, muted: false, audio_output: None, show_sidebar: true, frame_gen: framegen::Settings::default() }
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
                "audio_output" if !value.is_empty() => config.audio_output = Some(value.to_string()),
                "show_sidebar" => config.show_sidebar = value == "true",
                "frame_gen" => config.frame_gen.enabled = value == "true",
                "frame_gen_multiplier" => {
                    config.frame_gen.multiplier = value.parse().unwrap_or(config.frame_gen.multiplier).clamp(2, 4)
                }
                "frame_gen_flow_scale" => {
                    config.frame_gen.flow_scale = value.parse().unwrap_or(config.frame_gen.flow_scale).clamp(0.25, 1.0)
                }
                "frame_gen_performance" => config.frame_gen.performance_mode = value == "true",
                "frame_gen_hdr" => config.frame_gen.hdr_mode = value == "true",
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
            "video_mode={}\nhardware_decode={}\nvolume={:.3}\nmuted={}\naudio_output={}\nshow_sidebar={}\n\
             frame_gen={}\nframe_gen_multiplier={}\nframe_gen_flow_scale={:.2}\nframe_gen_performance={}\nframe_gen_hdr={}\n",
            self.video_mode.as_deref().unwrap_or(""),
            self.hardware_decode,
            self.volume,
            self.muted,
            self.audio_output.as_deref().unwrap_or(""),
            self.show_sidebar,
            self.frame_gen.enabled,
            self.frame_gen.multiplier,
            self.frame_gen.flow_scale,
            self.frame_gen.performance_mode,
            self.frame_gen.hdr_mode,
        );
        if let Err(e) = fs::write(&path, text) {
            eprintln!("failed to save {}: {e}", path.display());
        }
    }
}
