//! Dock settings, stored as TOML in %APPDATA%\Lumen\dock.toml.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Icon edge length in DIPs (scaled by the monitor's DPI).
    pub icon_size: f32,
    /// Gap between the dock and the bottom of the work area, in DIPs.
    pub margin: f32,
    /// Hide the dock while a maximized window is on its monitor.
    pub hide_when_maximized: bool,
    /// How long the cursor must rest on the bottom edge before the dock pops up.
    pub reveal_delay_ms: u32,
    /// How long after the cursor leaves before a revealed dock hides again.
    pub hide_delay_ms: u32,
    /// Show running apps that are not pinned.
    pub show_running: bool,
    pub pins: Vec<Pin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pin {
    pub name: String,
    /// A path to an .exe or .lnk, or `shell:AppsFolder\<AppUserModelID>` for packaged apps.
    /// %ENV% variables are expanded.
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Process name used to match running windows (e.g. "WindowsTerminal.exe").
    /// Derived from `target` when it is an .exe or .lnk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            icon_size: 48.0,
            margin: 8.0,
            hide_when_maximized: true,
            reveal_delay_ms: 120,
            hide_delay_ms: 450,
            show_running: true,
            pins: default_pins(),
        }
    }
}

fn pin(name: &str, target: &str, exe: Option<&str>) -> Pin {
    Pin { name: name.into(), target: target.into(), args: String::new(), exe: exe.map(Into::into) }
}

fn default_pins() -> Vec<Pin> {
    vec![
        pin("File Explorer", r"%WINDIR%\explorer.exe", None),
        pin("Microsoft Edge", r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe", None),
        pin("Terminal", r"shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App", Some("WindowsTerminal.exe")),
        pin("Notepad", r"shell:AppsFolder\Microsoft.WindowsNotepad_8wekyb3d8bbwe!App", Some("Notepad.exe")),
        pin(
            "Settings",
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel",
            Some("SystemSettings.exe"),
        ),
    ]
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Lumen")
}

pub fn path() -> PathBuf {
    dir().join("dock.toml")
}

const HEADER: &str = "# Lumen dock settings. Saved changes apply immediately.\n\
# target: an .exe/.lnk path or shell:AppsFolder\\<AppUserModelID>; %ENV% vars are expanded.\n\
# exe: process name used to match running windows when target isn't an .exe/.lnk.\n\n";

/// Loads the config, writing the defaults on first run. A broken file falls back to the defaults
/// without overwriting it, so a typo never wipes the user's pins.
pub fn load() -> Config {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("lumen-bar: {} is invalid, using defaults: {e}", p.display());
                Config::default()
            }
        },
        Err(_) => {
            let cfg = Config::default();
            save(&cfg);
            cfg
        }
    }
}

pub fn save(cfg: &Config) {
    let _ = std::fs::create_dir_all(dir());
    if let Ok(text) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(path(), format!("{HEADER}{text}"));
    }
}
