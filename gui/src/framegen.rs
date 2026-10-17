//! Frame generation through lsfg-vk (Lossless Scaling's LSFG on Linux).
//!
//! lsfg-vk is a Vulkan layer, so it only sees this window when GTK renders
//! with Vulkan; `main` picks the renderer at startup.  The layer matches its
//! config profile by executable name and re-reads the file when the swapchain
//! is recreated, which the window forces after each change, so everything but
//! the renderer switch applies live.  A multiplier of 1 passes frames through
//! untouched, which is how "off" works without a restart.

use std::fs;
use std::path::{Path, PathBuf};

use gtk::glib;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

/// The lsfg-vk profile this app owns, matched against the process name.
pub const PROFILE: &str = "elgato4k-gui";

const LAYER_NAME: &str = "VK_LAYER_LS_frame_generation";
const DLL_SUFFIX: &str = "Steam/steamapps/common/Lossless Scaling/Lossless.dll";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub multiplier: u32,
    /// Resolution of the optical-flow pass, 0.25–1.0.
    pub flow_scale: f64,
    pub performance_mode: bool,
    pub hdr_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, multiplier: 2, flow_scale: 0.8, performance_mode: false, hdr_mode: false }
    }
}

/// Whether lsfg-vk is hooked into this process: GTK renders with Vulkan
/// and `main` didn't disable the layer.
pub fn renderer_ready() -> bool {
    std::env::var("GSK_RENDERER").is_ok_and(|r| r == "vulkan") && std::env::var_os("DISABLE_LSFG").is_none()
}

/// Why frame generation can't be used, if it can't.
pub fn unavailable_reason() -> Option<&'static str> {
    if !layer_installed() {
        Some("lsfg-vk is not installed")
    } else if dll_path().is_none() {
        Some("Lossless Scaling is not installed in Steam")
    } else {
        None
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("LSFG_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| glib::user_config_dir().join("lsfg-vk").join("conf.toml"))
}

fn read_config() -> DocumentMut {
    fs::read_to_string(config_path()).ok().and_then(|s| s.parse().ok()).unwrap_or_default()
}

fn layer_installed() -> bool {
    let mut dirs = vec![glib::user_data_dir()];
    dirs.extend(glib::system_data_dirs());
    dirs.push("/etc".into());
    dirs.iter().any(|dir| {
        let Ok(entries) = fs::read_dir(dir.join("vulkan/implicit_layer.d")) else { return false };
        entries.flatten().any(|e| fs::read_to_string(e.path()).is_ok_and(|s| s.contains(LAYER_NAME)))
    })
}

/// Where lsfg-vk will find Lossless.dll: the configured path, or Steam's.
fn dll_path() -> Option<PathBuf> {
    if let Some(dll) = std::env::var_os("LSFG_DLL_PATH") {
        return Some(dll.into()).filter(|p: &PathBuf| p.is_file());
    }
    if let Some(dll) = read_config().get("global").and_then(|g| g.get("dll")).and_then(Item::as_str) {
        return Some(dll.into()).filter(|p: &PathBuf| p.is_file());
    }
    let home = glib::home_dir();
    [glib::user_data_dir(), home.join(".var/app/com.valvesoftware.Steam/.local/share")]
        .iter()
        .map(|base| base.join(DLL_SUFFIX))
        .chain([home.join(".steam/steam").join(DLL_SUFFIX.trim_start_matches("Steam/"))])
        .find(|p| p.is_file())
}

/// Write this app's profile into the lsfg-vk config, keeping everything else.
pub fn write_profile(settings: &Settings) {
    let path = config_path();
    let mut doc = read_config();
    if !doc.contains_key("version") {
        doc["version"] = value(1);
    }

    let mut profile = Table::new();
    profile["exe"] = value(PROFILE);
    profile["multiplier"] = value(if settings.enabled { settings.multiplier as i64 } else { 1 });
    profile["flow_scale"] = value((settings.flow_scale * 100.0).round() / 100.0);
    profile["performance_mode"] = value(settings.performance_mode);
    profile["hdr_mode"] = value(settings.hdr_mode);

    let games = doc
        .entry("game")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut();
    let Some(games) = games else {
        eprintln!("{}: `game` is not an array of tables, not touching it", path.display());
        return;
    };
    let existing = games.iter().position(|t| t.get("exe").and_then(Item::as_str) == Some(PROFILE));
    match existing.and_then(|i| games.get_mut(i)) {
        Some(table) => *table = profile,
        None => games.push(profile),
    }

    if let Err(e) = write_atomic(&path, &doc.to_string()) {
        eprintln!("failed to save {}: {e}", path.display());
    }
}

/// lsfg-vk watches the file, so never let it see a half-written one.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}
