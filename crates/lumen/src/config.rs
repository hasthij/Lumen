//! User settings at %APPDATA%\Lumen\config.toml. Missing or broken files fall back to defaults.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub modules: Modules,
    pub clock: ClockConfig,
    pub launcher: LauncherConfig,
    pub appearance: Appearance,
    pub taskbar: TaskbarConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    pub clock: bool,
    pub launcher: bool,
    pub taskbar: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// Rainmeter skin to render, as `Config\File.ini` under the Skins folder.
    pub skin: String,
    /// Skins folder. Empty means "read SkinPath from Rainmeter.ini".
    pub skins_path: String,
    /// Window position in skin (96-DPI) pixels. `None` means "use Rainmeter.ini's WindowX/WindowY".
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Size multiplier over the skin's own scale (1.0 = as the skin defines it).
    pub size: f32,
    /// 24-hour time. `None` keeps the skin's setting.
    pub hour24: Option<bool>,
    /// Text and line colour as #RRGGBB. Empty keeps the skin's colours.
    pub color: String,
    /// Window opacity, 0.1-1.0.
    pub opacity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Appearance {
    /// A preset id from theme::PRESETS, or "custom".
    pub theme: String,
    /// Custom tint as #RRGGBB (used when theme = "custom").
    pub tint: String,
    /// 0 = pure backdrop, 1 = solid tint.
    pub strength: f32,
    /// "mica", "mica_alt", "acrylic" or "solid".
    pub backdrop: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme: "graphite".into(), tint: "#202022".into(), strength: 0.45, backdrop: "mica".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TaskbarLook {
    /// "clear", "tinted", "blur" or "normal".
    pub mode: String,
    /// Tint colour as #RRGGBB.
    pub color: String,
    /// Tint opacity, 0-1.
    pub opacity: f32,
}

impl Default for TaskbarLook {
    fn default() -> Self {
        Self { mode: "clear".into(), color: "#000000".into(), opacity: 0.3 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TaskbarConfig {
    /// On the desktop or with ordinary windows open.
    pub desktop: TaskbarLook,
    /// When a window is maximized.
    pub maximized: TaskbarLook,
    /// While Start or Search is open.
    pub start: TaskbarLook,
    /// While Task View is open.
    pub task_view: TaskbarLook,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        // Mirrors the TranslucentTB setup this replaces: clear, but normal in Task View.
        let normal = TaskbarLook { mode: "normal".into(), ..Default::default() };
        Self { desktop: TaskbarLook::default(), maximized: TaskbarLook::default(), start: TaskbarLook::default(), task_view: normal }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            modules: Modules::default(),
            clock: ClockConfig::default(),
            launcher: LauncherConfig::default(),
            appearance: Appearance::default(),
            taskbar: TaskbarConfig::default(),
        }
    }
}

impl Default for Modules {
    fn default() -> Self {
        Self { clock: true, launcher: true, taskbar: false }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            skin: r"Summit\Time\Time.ini".into(),
            skins_path: String::new(),
            x: None,
            y: None,
            size: 1.0,
            hour24: None,
            color: String::new(),
            opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherConfig {
    /// e.g. "Alt+Space", "Win+Alt+Space", "Ctrl+Shift+K".
    pub hotkey: String,
    pub max_results: usize,
    /// Don't open while a fullscreen game or presentation is in front.
    pub ignore_in_fullscreen: bool,
    /// Launcher size multiplier on top of the display scaling (1.0 = normal).
    pub scale: f32,
    /// Open/close animation: "none", "fade", "slide_up", "slide_down" or "zoom".
    pub animation: String,
    /// Animation length in milliseconds (kept short so it never feels slow).
    pub animation_ms: u32,
    /// Always offer "search the web" as the last result for what you typed.
    pub web_fallback: bool,
    /// A preset id from `ENGINES`, or "custom" to use `search_url`.
    pub search_engine: String,
    /// Used when `search_engine` is "custom"; `{}` is replaced by the query.
    pub search_url: String,
    pub plugins: BTreeMap<String, PluginSetting>,
}

/// (id, name, URL template). Opened in the default browser.
pub const ENGINES: &[(&str, &str, &str)] = &[
    ("google", "Google", "https://www.google.com/search?q={}"),
    ("bing", "Bing", "https://www.bing.com/search?q={}"),
    ("duckduckgo", "DuckDuckGo", "https://duckduckgo.com/?q={}"),
    ("brave", "Brave", "https://search.brave.com/search?q={}"),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginSetting {
    pub enabled: bool,
    /// Typing this first sends the query to this plugin only. Empty: no keyword.
    pub keyword: String,
    /// Also answer queries typed without the keyword.
    pub global: bool,
}

/// Defaults mirror the PowerToys Run setup this replaces.
pub const PLUGIN_DEFAULTS: &[(&str, &str, bool)] = &[
    ("apps", ".", true),
    ("folder", "", true),
    ("files", "?", true),
    ("calculator", "=", true),
    ("units", "%%", false),
    ("time", ")", true),
    ("values", "#", false),
    ("windows", "<", true),
    ("shell", ">", false),
    ("system", "", true),
    ("settings", "$", false),
    ("services", "!", false),
    ("registry", ":", false),
    ("web", "??", false),
];

impl Default for LauncherConfig {
    fn default() -> Self {
        let mut cfg = Self {
            hotkey: "Alt+Space".into(),
            max_results: 6,
            ignore_in_fullscreen: true,
            scale: 1.0,
            animation: "fade".into(),
            animation_ms: 140,
            web_fallback: true,
            search_engine: "google".into(),
            search_url: String::new(),
            plugins: BTreeMap::new(),
        };
        cfg.fill_defaults();
        cfg
    }
}

impl LauncherConfig {
    /// Adds any plugin missing from the file, so new plugins appear after updates.
    pub fn fill_defaults(&mut self) {
        for (id, keyword, global) in PLUGIN_DEFAULTS {
            self.plugins
                .entry((*id).to_string())
                .or_insert(PluginSetting { enabled: true, keyword: (*keyword).to_string(), global: *global });
        }
        self.max_results = self.max_results.clamp(1, 12);
        self.scale = self.scale.clamp(0.7, 2.0);
        self.animation_ms = self.animation_ms.clamp(0, 400);
    }

    pub fn plugin(&self, id: &str) -> Option<&PluginSetting> {
        self.plugins.get(id).filter(|p| p.enabled)
    }

    /// The web search URL for `query`, already percent-encoded.
    pub fn search_url_for(&self, query: &str) -> String {
        let template = match self.search_engine.as_str() {
            "custom" if self.search_url.contains("{}") => self.search_url.as_str(),
            id => ENGINES.iter().find(|e| e.0 == id).map_or(ENGINES[0].2, |e| e.2),
        };
        template.replace("{}", &crate::launcher::plugins::values::url_encode(query))
    }

    pub fn engine_name(&self) -> &str {
        match self.search_engine.as_str() {
            "custom" => "your search engine",
            id => ENGINES.iter().find(|e| e.0 == id).map_or("Google", |e| e.1),
        }
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
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    cfg.launcher.fill_defaults();
                    cfg
                }
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
