//! User settings at %APPDATA%\Lumen\config.toml. Missing or broken files fall back to defaults.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub modules: Modules,
    pub clock: ClockConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    pub clock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// Rainmeter skin to render, as `Config\File.ini` under the Skins folder.
    pub skin: String,
    /// Skins folder. Empty means "read SkinPath from Rainmeter.ini".
    pub skins_path: String,
    /// Window position in physical pixels. `None` means "use Rainmeter.ini's WindowX/WindowY".
    pub x: Option<i32>,
    pub y: Option<i32>,
}

impl Default for Config {
    fn default() -> Self {
        Self { modules: Modules::default(), clock: ClockConfig::default() }
    }
}

impl Default for Modules {
    fn default() -> Self {
        Self { clock: true }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self { skin: r"Summit\Time\Time.ini".into(), skins_path: String::new(), x: None, y: None }
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Lumen").join("config.toml")
}

impl Config {
    pub fn load() -> Self {
        let path = path();
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    crate::warn!("config.toml is invalid, using defaults: {e}");
                    let _ = fs::copy(&path, path.with_extension("toml.bad"));
                    Self::default()
                }
            },
            Err(_) => {
                let cfg = Self::default();
                cfg.save();
                cfg
            }
        }
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a half-written config.
    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let Ok(text) = toml::to_string_pretty(self) else { return };
        let tmp = path.with_extension("toml.tmp");
        if fs::write(&tmp, text).is_ok() {
            if let Err(e) = fs::rename(&tmp, &path) {
                crate::warn!("could not save config: {e}");
            }
        }
    }
}
