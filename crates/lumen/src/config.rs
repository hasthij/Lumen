//! User settings at %APPDATA%\Lumen\config.toml. Missing or broken files fall back to defaults.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub modules: Modules,
    /// Legacy single-skin settings. Only `skins_path` is still used; the skin itself
    /// moves into `skins` the first time an older config is loaded.
    pub clock: ClockConfig,
    /// Every skin Lumen knows about, with its own position and look. `enabled` is the switch.
    pub skins: Vec<ClockConfig>,
    pub launcher: LauncherConfig,
    pub appearance: Appearance,
    pub taskbar: TaskbarConfig,
    pub island: IslandConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    pub clock: bool,
    pub launcher: bool,
    pub taskbar: bool,
    pub island: bool,
}

/// The clipboard + media island docked to a screen edge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IslandConfig {
    /// "left", "right" or "top".
    pub side: String,
    /// Size multiplier on top of display scaling, 0.7-2.0.
    pub scale: f32,
    /// "none", "fade", "slide_up", "slide_down" or "zoom"; "liquid" is the spring pull.
    pub animation: String,
    /// Animation length in ms, 0-400 (0 = instant).
    pub animation_ms: u32,
    /// Background as #RRGGBB.
    pub bg: String,
    /// Background opacity, 0.4-1.0.
    pub opacity: f32,
    /// Thin light outline around the island.
    pub outline: bool,
    /// Days of clipboard history kept, 1-30.
    pub history_days: u32,
    /// Clicking a clipboard row also pastes it (Ctrl+V) into the focused app.
    pub paste_on_click: bool,
    /// Briefly show the peek pill when something is copied.
    pub copy_nudge: bool,
    /// How long the copy nudge stays out, 300-3000 ms.
    pub nudge_ms: u32,
    /// Clipboard pin, delete and search controls.
    pub clip_tools: bool,
    /// Keep copied images in the clipboard history.
    pub clip_images: bool,
    /// Volume and brightness sliders on the Media tab.
    pub media_controls: bool,
    /// Quick-launch tab (pinned apps and folders).
    pub tab_launch: bool,
    /// System glance tab (CPU, memory, battery).
    pub tab_system: bool,
    /// Sticky notes tab.
    pub tab_shelf: bool,
    /// Look preset: "smoke", "ink", "frost", "liquid", "aura" or "custom".
    pub theme: String,
    /// Glass tint: "dark" (uses bg), "light", "accent" or "clear".
    pub tint: String,
    /// Accent tint hue, 0-360.
    pub accent_hue: f32,
    /// Background blur radius in px, 0-60 (0 = solid).
    pub blur: u32,
    /// Blurred backdrop saturation in %, 50-250.
    pub saturation: u32,
    /// Edge highlight strength, 0-0.6.
    pub edge: f32,
    /// Open-panel corner radius, 12-40.
    pub radius: f32,
    /// Fine noise over the glass.
    pub grain: bool,
    /// Soft light sheen across the top.
    pub sheen: bool,
    /// UI font family.
    pub font: String,
    /// Base font size, 11-16.
    pub font_size: f32,
    /// Tab bar look: "pills", "segmented" or "underline".
    pub tab_style: String,
    /// Tab bar shows icons instead of words.
    pub tab_icons: bool,
}

impl Default for IslandConfig {
    fn default() -> Self {
        let mut c = Self {
            side: "right".into(),
            scale: 1.0,
            animation: "liquid".into(),
            animation_ms: 220,
            bg: "#000000".into(),
            opacity: 0.92,
            outline: true,
            history_days: 7,
            paste_on_click: true,
            copy_nudge: true,
            nudge_ms: 700,
            clip_tools: true,
            clip_images: false,
            media_controls: true,
            tab_launch: false,
            tab_system: false,
            tab_shelf: false,
            theme: String::new(),
            tint: String::new(),
            accent_hue: 205.0,
            blur: 0,
            saturation: 100,
            edge: 0.0,
            radius: 28.0,
            grain: false,
            sheen: false,
            font: String::new(),
            font_size: 13.0,
            tab_style: String::new(),
            tab_icons: false,
        };
        c.apply_preset("smoke");
        c
    }
}

pub const ISLAND_THEMES: [&str; 5] = ["smoke", "ink", "frost", "liquid", "aura"];
pub const ISLAND_FONTS: [&str; 5] = ["Segoe UI", "Inter", "Manrope", "Sora", "Outfit"];

impl IslandConfig {
    /// Sets the look knobs to a named preset (from the design lab).
    pub fn apply_preset(&mut self, name: &str) {
        // (tint, blur, opacity, sat, edge, radius, font, tabs, grain, sheen)
        let (tint, blur, op, sat, edge, radius, font, tabs, fx) = match name {
            "ink" => ("dark", 0, 0.95, 100, 0.06, 22.0, "Segoe UI", "pills", false),
            "frost" => ("light", 50, 0.55, 160, 0.40, 30.0, "Manrope", "segmented", true),
            "liquid" => ("clear", 24, 0.08, 220, 0.45, 36.0, "Sora", "pills", true),
            "aura" => ("accent", 44, 0.40, 200, 0.24, 30.0, "Outfit", "underline", true),
            _ => ("dark", 40, 0.38, 180, 0.18, 28.0, "Inter", "pills", true),
        };
        self.theme = if ISLAND_THEMES.contains(&name) { name } else { "smoke" }.into();
        self.tint = tint.into();
        self.blur = blur;
        self.opacity = op;
        self.saturation = sat;
        self.edge = edge;
        self.radius = radius;
        self.font = font.into();
        self.font_size = 13.0;
        self.tab_style = tabs.into();
        self.grain = fx;
        self.sheen = fx;
        if name == "ink" || name == "smoke" {
            self.bg = "#000000".into();
        }
    }

    pub fn clamp(&mut self) {
        if !matches!(self.side.as_str(), "left" | "right" | "top") {
            self.side = "right".into();
        }
        if !matches!(self.animation.as_str(), "none" | "liquid" | "fade" | "zoom") {
            self.animation = "liquid".into();
        }
        self.scale = self.scale.clamp(0.7, 2.0);
        self.animation_ms = self.animation_ms.clamp(0, 400);
        self.opacity = self.opacity.clamp(0.0, 1.0);
        if !matches!(self.tint.as_str(), "dark" | "light" | "accent" | "clear") {
            self.tint = "dark".into();
        }
        if !matches!(self.tab_style.as_str(), "pills" | "segmented" | "underline") {
            self.tab_style = "pills".into();
        }
        if !matches!(self.theme.as_str(), "smoke" | "ink" | "frost" | "liquid" | "aura" | "custom") {
            self.theme = "custom".into();
        }
        if self.font.trim().is_empty() || self.font.len() > 64 {
            self.font = "Segoe UI".into();
        }
        self.accent_hue = self.accent_hue.rem_euclid(360.0);
        self.blur = self.blur.min(60);
        self.saturation = self.saturation.clamp(50, 250);
        self.edge = self.edge.clamp(0.0, 0.6);
        self.radius = self.radius.clamp(12.0, 40.0);
        self.font_size = self.font_size.clamp(11.0, 16.0);
        self.history_days = self.history_days.clamp(1, 30);
        self.nudge_ms = self.nudge_ms.clamp(300, 3000);
        if self.bg.len() != 7 || !self.bg.starts_with('#') || u32::from_str_radix(&self.bg[1..], 16).is_err() {
            self.bg = "#000000".into();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// Rainmeter skin to render, as `Config\File.ini` under the Skins folder.
    pub skin: String,
    /// Shown on the desktop. Turned-off skins keep their position and look.
    pub enabled: bool,
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
            skins: Vec::new(),
            launcher: LauncherConfig::default(),
            appearance: Appearance::default(),
            taskbar: TaskbarConfig::default(),
            island: IslandConfig::default(),
        }
    }
}

impl Default for Modules {
    fn default() -> Self {
        Self { clock: true, launcher: true, taskbar: false, island: false }
    }
}

impl ClockConfig {
    /// Defaults for a skin turned on from the library: nothing overridden, the skin's own look.
    pub fn fresh() -> Self {
        Self { skin: String::new(), ..Self::default() }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            skin: r"Summit\Time\Time.ini".into(),
            enabled: true,
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
                    cfg.island.clamp();
                    cfg.migrate_skins(&text);
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

    /// Older configs had one `[clock]` skin. Move it into the skin list once.
    fn migrate_skins(&mut self, text: &str) {
        let has_list = text.contains("[[skins]]");
        if !has_list && self.skins.is_empty() && !self.clock.skin.is_empty() {
            let mut first = self.clock.clone();
            first.skins_path.clear();
            self.skins.push(first);
            crate::info!("moved the clock skin into the skin list");
        }
        // The legacy block now only carries the skins folder override.
        self.clock.skin.clear();
        self.skins.dedup_by(|a, b| a.skin.eq_ignore_ascii_case(&b.skin));
    }

    pub fn skin_mut(&mut self, rel: &str) -> Option<&mut ClockConfig> {
        self.skins.iter_mut().find(|s| s.skin.eq_ignore_ascii_case(rel))
    }

    pub fn skin(&self, rel: &str) -> Option<&ClockConfig> {
        self.skins.iter().find(|s| s.skin.eq_ignore_ascii_case(rel))
    }

    /// Switches a skin on or off, adding it to the list the first time it is turned on.
    pub fn set_skin_enabled(&mut self, rel: &str, on: bool) {
        if let Some(s) = self.skin_mut(rel) {
            s.enabled = on;
        } else if on {
            self.skins.push(ClockConfig { skin: rel.to_string(), ..ClockConfig::fresh() });
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
