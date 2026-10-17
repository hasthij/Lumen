//! Settings window: Windows 11 style (Mica, dark), created when opened and destroyed when
//! closed, so it costs nothing the rest of the time. Every change applies immediately;
//! sliders preview live while dragged and are saved when released.

mod canvas;

use windows::core::{w, Result, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Config, TaskbarLook, PLUGIN_DEFAULTS};
use crate::launcher::editor::Editor;
use crate::launcher::item::Kind;
use crate::theme::{self, Theme};
use canvas::{rgba, white, Align, Canvas, Color, Font, Rect};

const W: f32 = 960.0;
const H: f32 = 680.0;
const SIDEBAR: f32 = 240.0;
const CONTENT_X: f32 = SIDEBAR + 28.0;
const CONTENT_W: f32 = W - CONTENT_X - 32.0;
const HEADER_H: f32 = 96.0;
const RIGHT: f32 = CONTENT_X + CONTENT_W - 18.0;
const TIMER_CARET: usize = 1;
const TIMER_STATUS: usize = 2;
const TIMER_ANIM: usize = 3;
const TIMER_NOTICE: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    General,
    Appearance,
    Skins,
    Launcher,
    Taskbar,
    About,
}

const PAGES: &[(Page, &str, char)] = &[
    (Page::General, "General", '\u{E80F}'),
    (Page::Appearance, "Appearance", '\u{E790}'),
    (Page::Skins, "Skins", '\u{E8B7}'),
    (Page::Launcher, "Launcher", '\u{E721}'),
    (Page::Taskbar, "Taskbar", '\u{E7C4}'),
    (Page::About, "About", '\u{E946}'),
];

/// The four taskbar situations, in the order shown.
const SITUATIONS: &[(Sit, &str, &str)] = &[
    (Sit::Desktop, "On the desktop", "No window is maximized"),
    (Sit::Maximized, "Window maximized", "A window fills the screen"),
    (Sit::Start, "Start or Search open", ""),
    (Sit::TaskView, "Task View open", ""),
];

const MODES: &[(&str, &str)] = &[("clear", "Clear"), ("tinted", "Tinted"), ("blur", "Blur"), ("normal", "Normal")];

/// Colour swatches offered in pickers: neutrals plus a spread of hues.
const SWATCHES: &[u32] = &[0xFFFFFF, 0xC8C8C8, 0x000000, 0x6EA8FF, 0xB18CFF, 0xFF8FB3, 0xFF6B6B, 0xFFBF5E, 0x6FD39A, 0x5ED4E0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sit {
    Desktop,
    Maximized,
    Start,
    TaskView,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Slider {
    Strength,
    TintHue,
    TintLight,
    ClockSize,
    ClockOpacity,
    LauncherScale,
    AnimSpeed,
    ClockHue,
    ClockSat,
    TbOpacity(Sit),
    TbHue(Sit),
    TbLight(Sit),
}

#[derive(Clone, PartialEq, Debug)]
enum Hit {
    Nav(Page),
    Autostart,
    Module(&'static str),
    Fullscreen,
    Hotkey,
    MaxMinus,
    MaxPlus,
    PluginEnabled(String),
    PluginGlobal(String),
    Keyword(String),
    ReloadSkin,
    OpenLog,
    OpenConfig,
    OpenSkins,
    WebFallback,
    Animation(&'static str),
    Engine(&'static str),
    Slider(Slider, Rect),
    Theme(&'static str),
    Backdrop(&'static str),
    ClockColor(Option<u32>),
    Hour(Option<bool>),
    MoveClock,
    ResetClock,
    Expand(String),
    SkinToggle(String),
    SelectSkin(String),
    RefreshSkins,
    TbMode(Sit, &'static str),
    TbColor(Sit, u32),
    QuitTtb,
    RestoreTaskbar,
    RestartExplorer,
}

/// What the app should do after a settings interaction.
pub enum Change {
    Config(Box<Config>),
    /// Apply without saving (a slider is being dragged).
    Preview(Box<Config>),
    Autostart(bool),
    ReloadSkin,
    /// Pause the global hotkey while the user records a new one.
    RecordingHotkey(bool),
    /// Start (Some) or stop (None) dragging a skin on the desktop.
    MoveSkin(Option<String>),
    QuitTranslucentTB,
    RestoreTaskbar,
    RestartExplorer,
}

/// Live facts the app reports to the settings window.
#[derive(Default, Clone)]
pub struct Status {
    pub hotkey_ok: bool,
    /// How many skin windows are running.
    pub skins_running: usize,
    /// Per running skin: its path and a note about what Lumen can't draw yet (empty = all good).
    pub skin_notes: Vec<(String, String)>,
    pub memory_mb: f32,
    pub autostart: bool,
    pub apps: usize,
    pub taskbar: String,
    pub translucenttb: bool,
    /// The skin being dragged right now, or empty.
    pub moving_skin: String,
    /// Where Rainmeter's skins live (the Skins folder the clock reads from).
    pub skins_dir: String,
}

pub struct Settings {
    hwnd: HWND,
    canvas: Canvas,
    page: Page,
    cfg: Config,
    status: Status,
    regions: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    editing: Option<String>,
    editor: Editor,
    caret_on: bool,
    recording: bool,
    dragging: Option<(Slider, Rect)>,
    scroll: f32,
    content_h: f32,
    scale: f32,
    changes: Vec<Change>,
    pub closed: bool,
    theme: Theme,
    backdrop: bool,
    /// Toggle knob positions (0 = off, 1 = on), eased toward their target each frame.
    anim: std::collections::HashMap<String, f32>,
    tree: Vec<crate::rainmeter::Node>,
    expanded: std::collections::HashSet<String>,
    /// The skin whose options are shown below the library.
    selected: Option<String>,
    animating: bool,
    /// Short confirmation shown next to a button ("Reloaded"), cleared by a timer.
    notice: Option<String>,
    /// Page fade-in progress, 0 to 1.
    page_fade: f32,
}

impl Settings {
    pub fn open(cfg: &Config, status: Status) -> Result<Self> {
        let hwnd = unsafe { create_window()? };
        let backdrop = theme::apply_backdrop(hwnd, &cfg.appearance, false);
        let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
        unsafe {
            // Size the client area to W x H at this DPI, centred on the work area.
            let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
            let mut r = RECT { left: 0, top: 0, right: (W * scale) as i32, bottom: (H * scale) as i32 };
            let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE(0), (scale * 96.0) as u32);
            let (ww, wh) = (r.right - r.left, r.bottom - r.top);
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut info);
            let wa = info.rcWork;
            let _ = SetWindowPos(hwnd, None, wa.left + (wa.right - wa.left - ww) / 2, wa.top + (wa.bottom - wa.top - wh) / 2, ww, wh, SWP_NOZORDER);
            let icon = crate::icon::create();
            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(icon.0 as isize));
            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(icon.0 as isize));
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            SetTimer(hwnd, TIMER_STATUS, 2000, None);
        }
        let mut me = Self::new(hwnd, cfg, status, scale, backdrop);
        me.refresh_library();
        Ok(me)
    }

    fn new(hwnd: HWND, cfg: &Config, status: Status, scale: f32, backdrop: bool) -> Self {
        Self {
            hwnd,
            canvas: Canvas::new().expect("Direct2D is part of Windows"),
            page: Page::General,
            cfg: cfg.clone(),
            status,
            regions: Vec::new(),
            hover: None,
            editing: None,
            editor: Editor::default(),
            caret_on: true,
            recording: false,
            dragging: None,
            scroll: 0.0,
            content_h: 0.0,
            scale,
            changes: Vec::new(),
            closed: false,
            theme: theme::resolve(&cfg.appearance),
            backdrop,
            anim: std::collections::HashMap::new(),
            tree: Vec::new(),
            expanded: std::collections::HashSet::new(),
            selected: None,
            animating: false,
            notice: None,
            page_fade: 1.0,
        }
    }

    /// Re-reads the Skins folder. Folders holding a running skin start expanded.
    fn refresh_library(&mut self) {
        let dir = std::path::PathBuf::from(&self.status.skins_dir);
        self.tree = if self.status.skins_dir.is_empty() { Vec::new() } else { crate::rainmeter::scan(&dir) };
        for sk in self.cfg.skins.iter().filter(|k| k.enabled) {
            let mut path = String::new();
            let parts: Vec<&str> = sk.skin.split('\\').collect();
            for part in &parts[..parts.len().saturating_sub(1)] {
                if !path.is_empty() {
                    path.push('\\');
                }
                path.push_str(part);
                self.expanded.insert(path.clone());
            }
        }
        if self.selected.as_ref().map_or(true, |r| !self.cfg.skin(r).is_some_and(|k| k.enabled)) {
            self.selected = self.cfg.skins.iter().find(|k| k.enabled).map(|k| k.skin.clone());
        }
    }

    /// Settings of the selected skin (defaults when none is selected).
    fn sel(&self) -> crate::config::ClockConfig {
        self.selected.as_deref().and_then(|r| self.cfg.skin(r)).cloned().unwrap_or_default()
    }

    fn sel_mut(&mut self) -> Option<&mut crate::config::ClockConfig> {
        let r = self.selected.clone()?;
        self.cfg.skin_mut(&r)
    }

    pub fn focus(&self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    pub fn set_status(&mut self, status: Status) {
        self.status = status;
        self.redraw();
    }

    /// The app changed the configuration (tray menu, clock dragged).
    pub fn sync(&mut self, cfg: &Config) {
        if self.dragging.is_none() && self.editing.is_none() {
            self.cfg = cfg.clone();
            self.redraw();
        }
    }

    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    fn redraw(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn appearance_changed(&mut self) {
        self.theme = theme::resolve(&self.cfg.appearance);
        self.backdrop = theme::apply_backdrop(self.hwnd, &self.cfg.appearance, false);
    }

    fn commit_config(&mut self) {
        self.appearance_changed();
        self.changes.push(Change::Config(Box::new(self.cfg.clone())));
        self.redraw();
    }

    fn preview_config(&mut self) {
        self.appearance_changed();
        self.changes.push(Change::Preview(Box::new(self.cfg.clone())));
        self.redraw();
    }

    fn look_mut(&mut self, s: Sit) -> &mut TaskbarLook {
        match s {
            Sit::Desktop => &mut self.cfg.taskbar.desktop,
            Sit::Maximized => &mut self.cfg.taskbar.maximized,
            Sit::Start => &mut self.cfg.taskbar.start,
            Sit::TaskView => &mut self.cfg.taskbar.task_view,
        }
    }

    fn look(&self, s: Sit) -> &TaskbarLook {
        match s {
            Sit::Desktop => &self.cfg.taskbar.desktop,
            Sit::Maximized => &self.cfg.taskbar.maximized,
            Sit::Start => &self.cfg.taskbar.start,
            Sit::TaskView => &self.cfg.taskbar.task_view,
        }
    }

    // ---- slider values -------------------------------------------------------------

    /// Current value of a slider, 0-1 along its track.
    fn slider_value(&self, s: Slider) -> f32 {
        let hsv_of = |hex: &str, dflt: [f32; 3]| theme::to_hsv(theme::parse_hex(hex).unwrap_or(dflt));
        match s {
            Slider::Strength => self.cfg.appearance.strength,
            Slider::TintHue => hsv_of(&self.cfg.appearance.tint, [0.12; 3]).0 / 360.0,
            Slider::TintLight => hsv_of(&self.cfg.appearance.tint, [0.12; 3]).2 / 0.4,
            Slider::ClockSize => (self.sel().size - 0.5) / 2.0,
            Slider::ClockOpacity => (self.sel().opacity - 0.1) / 0.9,
            Slider::LauncherScale => (self.cfg.launcher.scale - 0.7) / 1.3,
            Slider::AnimSpeed => self.cfg.launcher.animation_ms as f32 / 400.0,
            Slider::ClockHue => hsv_of(&self.sel().color, [1.0; 3]).0 / 360.0,
            Slider::ClockSat => hsv_of(&self.sel().color, [1.0; 3]).1,
            Slider::TbOpacity(s) => self.look(s).opacity,
            Slider::TbHue(s) => hsv_of(&self.look(s).color, [0.0; 3]).0 / 360.0,
            Slider::TbLight(s) => hsv_of(&self.look(s).color, [0.0; 3]).2,
        }
        .clamp(0.0, 1.0)
    }

    fn set_slider(&mut self, s: Slider, t: f32) {
        let t = t.clamp(0.0, 1.0);
        let hsv_of = |hex: &str, dflt: [f32; 3]| theme::to_hsv(theme::parse_hex(hex).unwrap_or(dflt));
        match s {
            Slider::Strength => self.cfg.appearance.strength = (t * 100.0).round() / 100.0,
            Slider::TintHue | Slider::TintLight => {
                let (h, _, v) = hsv_of(&self.cfg.appearance.tint, [0.12; 3]);
                let (h, v) = if s == Slider::TintHue { (t * 360.0, v.max(0.08)) } else { (h, t * 0.4) };
                self.cfg.appearance.tint = theme::to_hex(theme::hsv(h, 0.6, v));
                self.cfg.appearance.theme = "custom".into();
            }
            Slider::ClockSize => {
                if let Some(c) = self.sel_mut() {
                    c.size = ((0.5 + t * 2.0) * 20.0).round() / 20.0;
                }
            }
            Slider::ClockOpacity => {
                if let Some(c) = self.sel_mut() {
                    c.opacity = ((0.1 + t * 0.9) * 100.0).round() / 100.0;
                }
            }
            Slider::LauncherScale => self.cfg.launcher.scale = ((0.7 + t * 1.3) * 20.0).round() / 20.0,
            Slider::AnimSpeed => self.cfg.launcher.animation_ms = ((t * 400.0 / 10.0).round() * 10.0) as u32,
            Slider::ClockHue | Slider::ClockSat => {
                let (h, sat, _) = hsv_of(&self.sel().color, [1.0; 3]);
                let (h, sat) = if s == Slider::ClockHue { (t * 360.0, sat.max(0.35)) } else { (h, t) };
                if let Some(c) = self.sel_mut() {
                    c.color = theme::to_hex(theme::hsv(h, sat, 1.0));
                }
            }
            Slider::TbOpacity(sit) => self.look_mut(sit).opacity = (t * 100.0).round() / 100.0,
            Slider::TbHue(sit) | Slider::TbLight(sit) => {
                let (h, _, v) = hsv_of(&self.look(sit).color, [0.0; 3]);
                let (h, v) = if matches!(s, Slider::TbHue(_)) { (t * 360.0, v.max(0.2)) } else { (h, t) };
                self.look_mut(sit).color = theme::to_hex(theme::hsv(h, 0.65, v));
            }
        }
    }

    fn slider_label(&self, s: Slider) -> String {
        match s {
            Slider::Strength => format!("{}%", (self.cfg.appearance.strength * 100.0).round()),
            Slider::ClockSize => format!("{}%", (self.sel().size * 100.0).round()),
            Slider::ClockOpacity => format!("{}%", (self.sel().opacity * 100.0).round()),
            Slider::LauncherScale => format!("{}%", (self.cfg.launcher.scale * 100.0).round()),
            Slider::AnimSpeed => format!("{} ms", self.cfg.launcher.animation_ms),
            Slider::TbOpacity(sit) => format!("{}%", (self.look(sit).opacity * 100.0).round()),
            Slider::TintHue | Slider::ClockHue | Slider::TbHue(_) => "Hue".into(),
            Slider::TintLight | Slider::TbLight(_) => "Brightness".into(),
            Slider::ClockSat => "Saturation".into(),
        }
    }

    fn drag_to(&mut self, x: f32) {
        let Some((s, r)) = self.dragging else { return };
        self.set_slider(s, (x - r.x) / r.w);
        self.preview_config();
    }

    // ---- input ---------------------------------------------------------------------

    fn click(&mut self, hit: Hit, x: f32) {
        if self.editing.is_some() && !matches!(&hit, Hit::Keyword(_)) {
            self.finish_edit(true);
        }
        if self.recording && hit != Hit::Hotkey {
            self.stop_recording();
        }
        match hit {
            Hit::Nav(p) => {
                if self.page != p {
                    self.page_fade = 0.0;
                }
                self.page = p;
                self.scroll = 0.0;
            }
            Hit::Slider(s, r) => {
                self.dragging = Some((s, r));
                unsafe {
                    SetCapture(self.hwnd);
                }
                self.drag_to(x);
            }
            Hit::Autostart => self.changes.push(Change::Autostart(!self.status.autostart)),
            Hit::Module(m) => {
                match m {
                    "clock" => self.cfg.modules.clock = !self.cfg.modules.clock,
                    "launcher" => self.cfg.modules.launcher = !self.cfg.modules.launcher,
                    "taskbar" => self.cfg.modules.taskbar = !self.cfg.modules.taskbar,
                    _ => {}
                }
                self.commit_config();
            }
            Hit::Animation(id) => {
                self.cfg.launcher.animation = id.into();
                self.commit_config();
            }
            Hit::WebFallback => {
                self.cfg.launcher.web_fallback = !self.cfg.launcher.web_fallback;
                self.commit_config();
            }
            Hit::Engine(id) => {
                self.cfg.launcher.search_engine = id.into();
                self.commit_config();
            }
            Hit::Fullscreen => {
                self.cfg.launcher.ignore_in_fullscreen = !self.cfg.launcher.ignore_in_fullscreen;
                self.commit_config();
            }
            Hit::Hotkey => {
                if self.recording {
                    self.stop_recording();
                } else {
                    self.recording = true;
                    self.changes.push(Change::RecordingHotkey(true));
                }
            }
            Hit::MaxMinus | Hit::MaxPlus => {
                let n = self.cfg.launcher.max_results as i32 + if hit == Hit::MaxPlus { 1 } else { -1 };
                self.cfg.launcher.max_results = n.clamp(1, 12) as usize;
                self.commit_config();
            }
            Hit::PluginEnabled(id) => {
                if let Some(p) = self.cfg.launcher.plugins.get_mut(&id) {
                    p.enabled = !p.enabled;
                }
                self.commit_config();
            }
            Hit::PluginGlobal(id) => {
                if let Some(p) = self.cfg.launcher.plugins.get_mut(&id) {
                    p.global = !p.global;
                }
                self.commit_config();
            }
            Hit::Keyword(id) => {
                if self.editing.as_deref() != Some(id.as_str()) {
                    self.finish_edit(true);
                    let current = self.cfg.launcher.plugins.get(&id).map(|p| p.keyword.clone()).unwrap_or_default();
                    self.editor.set(&current);
                    self.editor.select_all();
                    self.editing = Some(id);
                    self.caret_on = true;
                    unsafe {
                        SetTimer(self.hwnd, TIMER_CARET, GetCaretBlinkTime().clamp(300, 1200), None);
                    }
                }
            }
            Hit::Theme(id) => {
                let was_black = self.cfg.appearance.theme == "black";
                self.cfg.appearance.theme = id.into();
                if id == "black" {
                    // OLED black: solid black with no backdrop showing through.
                    self.cfg.appearance.strength = 1.0;
                    self.cfg.appearance.backdrop = "solid".into();
                } else if was_black {
                    let d = crate::config::Appearance::default();
                    self.cfg.appearance.strength = d.strength;
                    self.cfg.appearance.backdrop = d.backdrop;
                }
                self.commit_config();
            }
            Hit::Backdrop(id) => {
                self.cfg.appearance.backdrop = id.into();
                self.commit_config();
            }
            Hit::ClockColor(c) => {
                if let Some(k) = self.sel_mut() {
                    k.color = c.map(|c| theme::to_hex(theme::rgb(c))).unwrap_or_default();
                }
                self.commit_config();
            }
            Hit::Hour(h) => {
                if let Some(k) = self.sel_mut() {
                    k.hour24 = h;
                }
                self.commit_config();
            }
            Hit::MoveClock => {
                let rel = self.selected.clone().unwrap_or_default();
                let moving_this = self.status.moving_skin.eq_ignore_ascii_case(&rel) && !rel.is_empty();
                self.status.moving_skin = if moving_this { String::new() } else { rel.clone() };
                self.changes.push(Change::MoveSkin(if moving_this { None } else { Some(rel) }));
            }
            Hit::ResetClock => {
                if let Some(k) = self.sel_mut() {
                    k.x = None;
                    k.y = None;
                    k.size = 1.0;
                    k.opacity = 1.0;
                    k.color.clear();
                    k.hour24 = None;
                }
                self.commit_config();
            }
            Hit::Expand(path) => {
                if !self.expanded.remove(&path) {
                    self.expanded.insert(path);
                }
            }
            Hit::SelectSkin(rel) => {
                if self.cfg.skin(&rel).is_some_and(|k| k.enabled) {
                    self.selected = Some(rel);
                }
            }
            Hit::SkinToggle(rel) => {
                let on = !self.cfg.skin(&rel).is_some_and(|k| k.enabled);
                self.cfg.set_skin_enabled(&rel, on);
                if on {
                    self.selected = Some(rel);
                } else if self.selected.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&rel)) {
                    self.selected = self.cfg.skins.iter().find(|k| k.enabled).map(|k| k.skin.clone());
                }
                self.commit_config();
            }
            Hit::RefreshSkins => self.refresh_library(),
            Hit::TbMode(sit, mode) => {
                self.look_mut(sit).mode = mode.into();
                self.commit_config();
            }
            Hit::TbColor(sit, c) => {
                self.look_mut(sit).color = theme::to_hex(theme::rgb(c));
                self.commit_config();
            }
            Hit::QuitTtb => self.changes.push(Change::QuitTranslucentTB),
            Hit::RestoreTaskbar => {
                self.cfg.modules.taskbar = false;
                self.changes.push(Change::RestoreTaskbar);
            }
            Hit::RestartExplorer => {
                self.cfg.modules.taskbar = false;
                self.changes.push(Change::RestartExplorer);
            }
            Hit::ReloadSkin => {
                self.changes.push(Change::ReloadSkin);
                self.notice = Some("Reloaded".into());
                unsafe {
                    SetTimer(self.hwnd, TIMER_NOTICE, 2500, None);
                }
            }
            Hit::OpenLog => open(&crate::log::dir().display().to_string()),
            Hit::OpenConfig => open(&crate::config::path().display().to_string()),
            Hit::OpenSkins => {
                if let Some(dir) = crate::rainmeter::Install::locate().skins {
                    open(&dir.display().to_string());
                }
            }
        }
        self.redraw();
    }

    fn finish_edit(&mut self, save: bool) {
        let Some(id) = self.editing.take() else { return };
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_CARET);
        }
        if save {
            let kw: String = self.editor.text().trim().chars().filter(|c| !c.is_whitespace()).take(4).collect();
            if let Some(p) = self.cfg.launcher.plugins.get_mut(&id) {
                if p.keyword != kw {
                    p.keyword = kw;
                    self.commit_config();
                }
            }
        }
        self.redraw();
    }

    fn stop_recording(&mut self) {
        if self.recording {
            self.recording = false;
            self.changes.push(Change::RecordingHotkey(false));
            self.redraw();
        }
    }

    /// While recording: turns the pressed combination into "Ctrl+Alt+K" style text.
    fn record_key(&mut self, vk: u16) {
        let key = VIRTUAL_KEY(vk);
        if matches!(key, VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_LWIN | VK_RWIN) {
            return;
        }
        if key == VK_ESCAPE {
            return self.stop_recording();
        }
        let down = |k: VIRTUAL_KEY| unsafe { GetKeyState(k.0 as i32) } < 0;
        let mut parts = Vec::new();
        if down(VK_LWIN) || down(VK_RWIN) {
            parts.push("Win".to_string());
        }
        if down(VK_CONTROL) {
            parts.push("Ctrl".into());
        }
        if down(VK_MENU) {
            parts.push("Alt".into());
        }
        if down(VK_SHIFT) {
            parts.push("Shift".into());
        }
        let name = match key {
            VK_SPACE => "Space".to_string(),
            VK_RETURN => "Enter".into(),
            VK_TAB => "Tab".into(),
            k if (VK_F1.0..=VK_F24.0).contains(&k.0) => format!("F{}", k.0 - VK_F1.0 + 1),
            k if (b'A' as u16..=b'Z' as u16).contains(&k.0) || (b'0' as u16..=b'9' as u16).contains(&k.0) => (k.0 as u8 as char).to_string(),
            _ => return,
        };
        let is_f = name.starts_with('F') && name.len() > 1;
        if parts.is_empty() && !is_f {
            return; // a bare letter would hijack normal typing
        }
        parts.push(name);
        self.cfg.launcher.hotkey = parts.join("+");
        self.recording = false;
        self.commit_config();
        self.changes.push(Change::RecordingHotkey(false));
    }

    fn key_down(&mut self, vk: u16) -> bool {
        if self.recording {
            self.record_key(vk);
            return true;
        }
        if self.editing.is_some() {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
            match VIRTUAL_KEY(vk) {
                VK_RETURN | VK_TAB => self.finish_edit(true),
                VK_ESCAPE => self.finish_edit(false),
                VK_BACK => self.editor.backspace(ctrl),
                VK_DELETE => self.editor.delete(ctrl),
                VK_LEFT => self.editor.left(ctrl, shift),
                VK_RIGHT => self.editor.right(ctrl, shift),
                VK_HOME => self.editor.move_to(0, shift),
                VK_END => {
                    let n = self.editor.len();
                    self.editor.move_to(n, shift)
                }
                VK_A if ctrl => self.editor.select_all(),
                _ => return false,
            }
            self.caret_on = true;
            self.redraw();
            return true;
        }
        if VIRTUAL_KEY(vk) == VK_ESCAPE {
            self.close();
            return true;
        }
        false
    }

    fn close(&mut self) {
        self.finish_edit(true);
        self.stop_recording();
        if !self.status.moving_skin.is_empty() {
            self.changes.push(Change::MoveSkin(None));
        }
        self.closed = true;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    fn mouse(&self, lp: LPARAM) -> (f32, f32) {
        ((lp.0 & 0xFFFF) as i16 as f32 / self.scale, ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / self.scale)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.regions.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone())
    }

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe { BeginPaint(self.hwnd, &mut ps) };
                let mut r = RECT::default();
                unsafe {
                    let _ = GetClientRect(self.hwnd, &mut r);
                }
                let px = ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32);
                if self.canvas.begin(self.hwnd, self.scale, px).is_ok() {
                    self.draw();
                    if let Err(e) = self.canvas.end() {
                        crate::warn!("settings draw failed: {e}");
                    }
                    // Frames only run while something is still moving, then the timer stops.
                    unsafe {
                        if std::mem::take(&mut self.animating) {
                            SetTimer(self.hwnd, TIMER_ANIM, 16, None);
                        } else {
                            let _ = KillTimer(self.hwnd, TIMER_ANIM);
                        }
                    }
                }
                unsafe {
                    let _ = EndPaint(self.hwnd, &ps);
                }
            }
            WM_LBUTTONDOWN => {
                let (x, y) = self.mouse(lp);
                match self.hit_at(x, y) {
                    Some(h) => self.click(h, x),
                    None => {
                        self.finish_edit(true);
                        self.stop_recording();
                    }
                }
            }
            WM_LBUTTONUP => {
                if self.dragging.take().is_some() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.commit_config();
                }
            }
            WM_MOUSEMOVE => {
                let (x, y) = self.mouse(lp);
                if self.dragging.is_some() {
                    self.drag_to(x);
                    return Some(LRESULT(0));
                }
                let h = self.hit_at(x, y);
                if h != self.hover {
                    self.hover = h;
                    self.redraw();
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
            }
            windows::Win32::UI::Controls::WM_MOUSELEAVE => {
                self.hover = None;
                self.redraw();
            }
            WM_MOUSEWHEEL => {
                let delta = (wp.0 >> 16) as i16 as f32;
                let max = (self.content_h - (H - HEADER_H)).max(0.0);
                self.scroll = (self.scroll - delta / 120.0 * 60.0).clamp(0.0, max);
                self.redraw();
            }
            WM_SETCURSOR => {
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT {
                    let id = match &self.hover {
                        Some(Hit::Keyword(_)) => IDC_IBEAM,
                        Some(_) => IDC_HAND,
                        None => IDC_ARROW,
                    };
                    unsafe {
                        if let Ok(c) = LoadCursorW(None, id) {
                            SetCursor(c);
                        }
                    }
                    return Some(LRESULT(1));
                }
                return None;
            }
            WM_KEYDOWN => {
                if !self.key_down(wp.0 as u16) {
                    return None;
                }
            }
            // Alt combinations while recording a shortcut must not open the window menu.
            WM_SYSKEYDOWN if self.recording => self.record_key(wp.0 as u16),
            WM_SYSCHAR if self.recording => {}
            WM_CHAR => {
                let c = wp.0 as u16;
                if self.editing.is_some() && c >= 0x20 && c != 0x7F {
                    self.editor.insert(&String::from_utf16_lossy(&[c]));
                    self.caret_on = true;
                    self.redraw();
                }
            }
            WM_TIMER => {
                if wp.0 == TIMER_CARET {
                    self.caret_on = !self.caret_on;
                    self.redraw();
                }
                if wp.0 == TIMER_NOTICE {
                    self.notice = None;
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_NOTICE);
                    }
                    self.redraw();
                }
                if wp.0 == TIMER_ANIM {
                    self.redraw();
                }
                // TIMER_STATUS is handled by the app, which owns the numbers.
                return None;
            }
            WM_DPICHANGED => {
                self.scale = ((wp.0 & 0xFFFF) as f32 / 96.0).max(1.0);
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.redraw();
            }
            WM_CLOSE => self.close(),
            WM_ERASEBKGND => return Some(LRESULT(1)),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    // ---- drawing -------------------------------------------------------------------

    fn accent(&self) -> Color {
        let [r, g, b] = self.theme.accent;
        rgba(r, g, b, 1.0)
    }

    fn draw(&mut self) {
        self.regions.clear();
        let [tr, tg, tb] = self.theme.tint;
        let strength = if self.backdrop { self.theme.strength } else { 1.0 };
        // Sidebar: the theme tint over Mica. Content: a touch lighter, like Windows Settings.
        self.canvas.fill(Rect::new(0.0, 0.0, W, H), 0.0, rgba(tr, tg, tb, strength * 0.85));
        self.canvas.fill(Rect::new(SIDEBAR, 0.0, W - SIDEBAR, H), 0.0, white(0.03));

        // App identity.
        let c = &mut self.canvas;
        c.fill(Rect::new(24.0, 26.0, 28.0, 28.0), 7.0, rgba(0.0, 0.0, 0.0, 1.0));
        c.stroke(Rect::new(24.0, 26.0, 28.0, 28.0), 7.0, 1.0, white(0.18));
        c.circle(38.0, 40.0, 5.5, white(1.0));
        c.text("Lumen", Rect::new(62.0, 26.0, 150.0, 28.0), Font::Display, 15.0, 600, white(0.95), Align::Left);

        let accent = self.accent();
        for (i, (page, name, glyph)) in PAGES.iter().enumerate() {
            let r = Rect::new(12.0, 80.0 + i as f32 * 40.0, SIDEBAR - 24.0, 36.0);
            let selected = self.page == *page;
            let hovered = self.hover == Some(Hit::Nav(*page));
            if selected || hovered {
                self.canvas.fill(r, 6.0, white(if selected { 0.07 } else { 0.04 }));
            }
            if selected {
                self.canvas.fill(Rect::new(r.x, r.y + 10.0, 3.0, 16.0), 1.5, accent);
            }
            self.canvas.glyph(*glyph, Rect::new(r.x + 14.0, r.y, 20.0, r.h), 15.0, white(0.9));
            self.canvas.text(name, Rect::new(r.x + 46.0, r.y, 140.0, r.h), Font::Text, 13.5, 400, white(0.92), Align::Left);
            self.regions.push((r, Hit::Nav(*page)));
        }

        let foot = format!("Using {:.1} MB", self.status.memory_mb);
        self.canvas.text(&foot, Rect::new(24.0, H - 44.0, SIDEBAR - 40.0, 24.0), Font::Text, 11.5, 400, white(0.45), Align::Left);

        let (title, subtitle) = match self.page {
            Page::General => ("General", "Startup and which modules are running"),
            Page::Appearance => ("Appearance", "Theme colour and backdrop for Lumen's windows"),
            Page::Skins => ("Skins", "Choose which Rainmeter skins stay on your desktop"),
            Page::Launcher => ("Launcher", "Search apps, files, settings and more"),
            Page::Taskbar => ("Taskbar", "Clear or tinted taskbar, by situation"),
            Page::About => ("About", "Lumen"),
        };
        self.canvas.text(title, Rect::new(CONTENT_X, 26.0, CONTENT_W, 40.0), Font::Display, 28.0, 600, white(0.96), Align::Left);
        self.canvas.text(subtitle, Rect::new(CONTENT_X, 62.0, CONTENT_W, 20.0), Font::Text, 12.5, 400, white(0.55), Align::Left);

        self.canvas.clip(Rect::new(SIDEBAR, HEADER_H, W - SIDEBAR, H - HEADER_H));
        let top = HEADER_H + 4.0 - self.scroll;
        let end = match self.page {
            Page::General => self.page_general(top),
            Page::Appearance => self.page_appearance(top),
            Page::Skins => self.page_skins(top),
            Page::Launcher => self.page_launcher(top),
            Page::Taskbar => self.page_taskbar(top),
            Page::About => self.page_about(top),
        };
        // A quick fade-in when the page changes: a veil in the window's own colour that thins out.
        if self.page_fade < 1.0 {
            self.page_fade = (self.page_fade + 0.14).min(1.0);
            let [tr, tg, tb] = self.theme.tint;
            let veil = (1.0 - self.page_fade) * 0.9;
            self.canvas.fill(Rect::new(SIDEBAR, HEADER_H, W - SIDEBAR, H - HEADER_H), 0.0, rgba(tr * 0.5, tg * 0.5, tb * 0.5, veil));
            self.animating = true;
        }
        self.canvas.unclip();
        self.content_h = end + self.scroll - HEADER_H + 24.0;
        // Regions scrolled out of view must not be clickable.
        self.regions.retain(|(r, h)| matches!(h, Hit::Nav(_)) || (r.y + r.h > HEADER_H && r.y < H));
    }

    fn section(&mut self, label: &str, y: f32) -> f32 {
        self.canvas.text(label, Rect::new(CONTENT_X, y, CONTENT_W, 28.0), Font::Text, 13.0, 600, white(0.85), Align::Left);
        y + 32.0
    }

    /// A Windows-Settings-style card with an icon, title and description.
    fn card(&mut self, y: f32, h: f32, glyph: Option<(char, Option<Kind>)>, title: &str, desc: &str) -> Rect {
        let r = Rect::new(CONTENT_X, y, CONTENT_W, h);
        self.canvas.fill(r, 6.0, white(0.05));
        self.canvas.stroke(r, 6.0, 1.0, white(0.05));
        let head_h = h.min(68.0);
        let mut tx = r.x + 18.0;
        if let Some((g, kind)) = glyph {
            match kind {
                Some(k) => {
                    let [cr, cg, cb] = k.tint();
                    let tile = Rect::new(r.x + 16.0, y + (head_h - 32.0) / 2.0, 32.0, 32.0);
                    self.canvas.fill(tile, 8.0, rgba(cr, cg, cb, 0.24));
                    let mix = |v: f32| v + (1.0 - v) * 0.45;
                    self.canvas.glyph(g, tile, 15.0, rgba(mix(cr), mix(cg), mix(cb), 1.0));
                    tx = r.x + 62.0;
                }
                None => {
                    self.canvas.glyph(g, Rect::new(r.x + 18.0, y, 22.0, head_h), 17.0, white(0.9));
                    tx = r.x + 56.0;
                }
            }
        }
        if !title.is_empty() {
            if desc.is_empty() {
                self.canvas.text(title, Rect::new(tx, y, 380.0, head_h), Font::Text, 14.0, 400, white(0.95), Align::Left);
            } else {
                self.canvas.text(title, Rect::new(tx, y + head_h / 2.0 - 20.0, 400.0, 20.0), Font::Text, 14.0, 400, white(0.95), Align::Left);
                self.canvas.text(desc, Rect::new(tx, y + head_h / 2.0, 420.0, 18.0), Font::Text, 12.0, 400, white(0.55), Align::Left);
            }
        }
        r
    }

    /// Windows 11 toggle switch, right-aligned at `right`.
    fn toggle(&mut self, right: f32, cy: f32, on: bool, hit: Hit, label: bool) -> Rect {
        let r = Rect::new(right - 40.0, cy - 10.0, 40.0, 20.0);
        let hovered = self.hover.as_ref() == Some(&hit);
        // Ease the knob toward its target; the first sighting starts in place (no animation).
        let target = if on { 1.0 } else { 0.0 };
        let key = format!("{hit:?}");
        let t = self.anim.entry(key.clone()).or_insert(target);
        if (*t - target).abs() > 0.01 {
            *t += (target - *t) * 0.3;
            self.animating = true;
        } else {
            *t = target;
        }
        let t = *t;
        let a = self.accent();
        let lerp = |x: f32, y: f32| x + (y - x) * t;
        // Track: outline fades into the accent fill as the knob travels.
        let fill = rgba(lerp(0.0, a.r), lerp(0.0, a.g), lerp(0.0, a.b), lerp(if hovered { 0.06 } else { 0.0 }, 1.0));
        self.canvas.fill(r, 10.0, fill);
        self.canvas.stroke(r, 10.0, 1.0, white(0.6 * (1.0 - t)));
        let knob = if hovered { 1.0 } else { 0.0 };
        let radius = lerp(5.0 + knob, 6.0 + knob);
        let color = rgba(lerp(0.8, 0.0), lerp(0.8, 0.0), lerp(0.8, 0.0), lerp(0.8, 0.85));
        self.canvas.circle(r.x + lerp(10.0, 30.0), cy, radius, color);
        let mut hit_rect = r;
        if label {
            self.canvas.text(if on { "On" } else { "Off" }, Rect::new(r.x - 44.0, cy - 10.0, 34.0, 20.0), Font::Text, 13.0, 400, white(0.9), Align::Right);
            hit_rect = Rect::new(r.x - 48.0, r.y - 6.0, r.w + 48.0, r.h + 12.0);
        }
        self.regions.push((hit_rect, hit));
        r
    }

    fn button(&mut self, right: f32, cy: f32, text: &str, hit: Hit) -> f32 {
        let w = self.canvas.measure(text, Font::Text, 13.0, 400) + 28.0;
        let r = Rect::new(right - w, cy - 16.0, w, 32.0);
        let hovered = self.hover.as_ref() == Some(&hit);
        self.canvas.fill(r, 5.0, white(if hovered { 0.1 } else { 0.07 }));
        self.canvas.stroke(r, 5.0, 1.0, white(0.07));
        self.canvas.text(text, r, Font::Text, 13.0, 400, white(0.92), Align::Center);
        self.regions.push((r, hit));
        r.x
    }

    /// Segmented control (like Windows' radio-button rows), right-aligned.
    fn segmented(&mut self, right: f32, cy: f32, options: &[(&str, Hit)], selected: usize) {
        let widths: Vec<f32> = options.iter().map(|(t, _)| self.canvas.measure(t, Font::Text, 12.5, 500) + 26.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 8.0;
        let outer = Rect::new(right - total, cy - 17.0, total, 34.0);
        self.canvas.fill(outer, 7.0, white(0.05));
        self.canvas.stroke(outer, 7.0, 1.0, white(0.07));
        let mut x = outer.x + 4.0;
        for (i, ((text, hit), w)) in options.iter().zip(&widths).enumerate() {
            let r = Rect::new(x, cy - 13.0, *w, 26.0);
            let hov = self.hover.as_ref() == Some(hit);
            if i == selected {
                self.canvas.fill(r, 5.0, white(0.12));
                self.canvas.fill(Rect::new(r.x + r.w / 2.0 - 8.0, r.y + r.h - 3.0, 16.0, 2.5), 1.25, self.accent());
            } else if hov {
                self.canvas.fill(r, 5.0, white(0.05));
            }
            self.canvas.text(text, r, Font::Text, 12.5, if i == selected { 600 } else { 400 }, white(if i == selected { 0.97 } else { 0.75 }), Align::Center);
            self.regions.push((r, hit.clone()));
            x += w;
        }
    }

    /// Horizontal slider. `track` paints the rail (e.g. a hue gradient) when given colours.
    fn slider(&mut self, x: f32, cy: f32, w: f32, s: Slider, rail: Option<&[Color]>) {
        let t = self.slider_value(s);
        let track = Rect::new(x, cy - 2.0, w, 4.0);
        match rail {
            Some(colors) if colors.len() > 1 => {
                let n = colors.len() - 1;
                let seg = w / n as f32;
                for (i, c) in colors.iter().take(n).enumerate() {
                    self.canvas.fill(Rect::new(x + i as f32 * seg, cy - 3.0, seg + 0.5, 6.0), 0.0, *c);
                }
            }
            _ => {
                self.canvas.fill(track, 2.0, white(0.25));
                self.canvas.fill(Rect::new(x, cy - 2.0, w * t, 4.0), 2.0, self.accent());
            }
        }
        let kx = x + w * t;
        let active = self.dragging.map(|d| d.0) == Some(s) || matches!(&self.hover, Some(Hit::Slider(h, _)) if *h == s);
        self.canvas.circle(kx, cy, 10.0, rgba(0.27, 0.27, 0.29, 1.0));
        self.canvas.stroke(Rect::new(kx - 10.0, cy - 10.0, 20.0, 20.0), 10.0, 1.0, white(0.12));
        self.canvas.circle(kx, cy, if active { 6.5 } else { 5.0 }, self.accent());
        let hit_rect = Rect::new(x - 10.0, cy - 14.0, w + 20.0, 28.0);
        self.regions.push((hit_rect, Hit::Slider(s, Rect::new(x, hit_rect.y, w, hit_rect.h))));
    }

    /// A labelled slider row inside a card: label left, slider and value right.
    fn slider_row(&mut self, y: f32, label: &str, s: Slider, rail: Option<&[Color]>) {
        self.canvas.text(label, Rect::new(CONTENT_X + 56.0, y, 200.0, 36.0), Font::Text, 13.0, 400, white(0.8), Align::Left);
        let value = self.slider_label(s);
        let show_value = value.ends_with('%');
        let w = 300.0;
        let x = RIGHT - w - if show_value { 56.0 } else { 0.0 };
        self.slider(x, y + 18.0, w, s, rail);
        if show_value {
            self.canvas.text(&value, Rect::new(RIGHT - 48.0, y, 48.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        }
    }

    fn swatch(&mut self, x: f32, cy: f32, color: Color, selected: bool, hit: Hit) {
        let hov = self.hover.as_ref() == Some(&hit);
        if selected {
            self.canvas.circle(x, cy, 14.0, self.accent());
        } else if hov {
            self.canvas.circle(x, cy, 14.0, white(0.3));
        }
        self.canvas.circle(x, cy, 11.0, rgba(0.1, 0.1, 0.1, 1.0));
        self.canvas.circle(x, cy, 10.0, color);
        self.regions.push((Rect::new(x - 14.0, cy - 14.0, 28.0, 28.0), hit));
    }

    fn hue_rail() -> Vec<Color> {
        (0..=12).map(|i| { let c = theme::hsv(i as f32 * 30.0, 0.75, 1.0); rgba(c[0], c[1], c[2], 1.0) }).collect()
    }

    // ---- pages ---------------------------------------------------------------------

    fn page_general(&mut self, mut y: f32) -> f32 {
        y = self.section("Startup", y);
        self.card(y, 68.0, Some(('\u{E7E8}', None)), "Start with Windows", "Open Lumen when you sign in");
        self.toggle(RIGHT, y + 34.0, self.status.autostart, Hit::Autostart, true);
        y += 68.0 + 24.0;

        y = self.section("Modules", y);
        let modules: [(&'static str, char, &str, &str, bool); 3] = [
            ("clock", '\u{E8B7}', "Skins", "Rainmeter skins on your desktop", self.cfg.modules.clock),
            ("launcher", '\u{E721}', "Launcher", "PowerToys Run replacement", self.cfg.modules.launcher),
            ("taskbar", '\u{E7C4}', "Clear taskbar", "TranslucentTB replacement", self.cfg.modules.taskbar),
        ];
        for (id, g, name, desc, on) in modules {
            self.card(y, 68.0, Some((g, None)), name, desc);
            self.toggle(RIGHT, y + 34.0, on, Hit::Module(id), true);
            y += 72.0;
        }
        y += 20.0;
        y = self.section("Troubleshooting", y);
        self.card(y, 68.0, Some(('\u{E9D9}', None)), "Logs and settings file", "For when something looks wrong");
        let x = self.button(RIGHT, y + 34.0, "Open log folder", Hit::OpenLog);
        self.button(x - 8.0, y + 34.0, "Open settings file", Hit::OpenConfig);
        y + 68.0
    }

    fn page_appearance(&mut self, mut y: f32) -> f32 {
        y = self.section("Theme", y);
        let card = self.card(y, 150.0, Some(('\u{E790}', None)), "Colour", "Tints the Mica backdrop of the launcher and this window");
        let mut x = card.x + 74.0;
        let cy = y + 100.0;
        for (id, name, tint, accent) in theme::PRESETS {
            let t = theme::resolve(&crate::config::Appearance { theme: (*id).into(), ..self.cfg.appearance.clone() });
            let a = if *accent == 0 { t.accent } else { theme::rgb(*accent) };
            let base = if *tint == 0 { t.tint } else { theme::rgb(*tint) };
            let selected = self.cfg.appearance.theme == *id;
            let hit = Hit::Theme(id);
            let hov = self.hover.as_ref() == Some(&hit);
            let r = Rect::new(x - 26.0, cy - 30.0, 52.0, 60.0);
            if selected || hov {
                self.canvas.fill(r, 8.0, white(if selected { 0.09 } else { 0.05 }));
            }
            if selected {
                self.canvas.stroke(r, 8.0, 1.5, self.accent());
            }
            // Two-tone dot: tint on the left, accent on the right.
            self.canvas.circle(x, cy - 8.0, 13.0, white(0.18));
            self.canvas.circle(x, cy - 8.0, 12.0, rgba((base[0] * 2.2).max(0.0), (base[1] * 2.2).max(0.0), (base[2] * 2.2).max(0.0), 1.0));
            self.canvas.circle(x + 6.0, cy - 8.0, 7.0, rgba(a[0], a[1], a[2], 1.0));
            let short = name.split(' ').next().unwrap_or(name);
            self.canvas.text(short, Rect::new(x - 28.0, cy + 10.0, 56.0, 16.0), Font::Text, 10.5, 400, white(0.75), Align::Center);
            self.regions.push((r, hit));
            x += 66.0;
        }
        y += 154.0;

        self.card(y, 142.0, Some(('\u{E771}', None)), "Custom colour", "Adjusting these switches to a custom theme");
        let rail = Self::hue_rail();
        self.slider_row(y + 64.0, "Hue", Slider::TintHue, Some(&rail));
        let tint = theme::parse_hex(&self.cfg.appearance.tint).unwrap_or([0.12; 3]);
        let (h, _, _) = theme::to_hsv(tint);
        let dark: Vec<Color> = (0..=4).map(|i| { let c = theme::hsv(h, 0.6, i as f32 * 0.1); rgba(c[0], c[1], c[2], 1.0) }).collect();
        self.slider_row(y + 98.0, "Brightness", Slider::TintLight, Some(&dark));
        y += 146.0;

        self.card(y, 68.0, Some(('\u{E793}', None)), "Tint strength", "0% shows pure Mica, 100% is a solid colour");
        let s = Slider::Strength;
        let value = self.slider_label(s);
        self.slider(RIGHT - 300.0 - 56.0, y + 34.0, 300.0, s, None);
        self.canvas.text(&value, Rect::new(RIGHT - 48.0, y + 16.0, 48.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        y += 72.0 + 20.0;

        y = self.section("Backdrop", y);
        self.card(y, 68.0, Some(('\u{E81E}', None)), "Material", "Mica is nearly free; Acrylic blurs live");
        let opts: Vec<(&str, Hit)> = theme::BACKDROPS.iter().map(|(id, name)| (*name, Hit::Backdrop(id))).collect();
        let sel = theme::BACKDROPS.iter().position(|(id, _)| *id == self.cfg.appearance.backdrop).unwrap_or(0);
        self.segmented(RIGHT, y + 34.0, &opts, sel);
        y + 68.0
    }

    /// The skin library (a folder tree like Rainmeter's manager) and the options of the selected skin.
    fn page_skins(&mut self, mut y: f32) -> f32 {
        y = self.section("Skins", y);
        self.card(y, 68.0, Some(('\u{E8B7}', None)), "Show skins", "Master switch for every skin below");
        self.toggle(RIGHT, y + 34.0, self.cfg.modules.clock, Hit::Module("clock"), true);
        y += 72.0;
        let dir = if self.status.skins_dir.is_empty() { "Rainmeter's Skins folder wasn't found".to_string() } else { self.status.skins_dir.clone() };
        self.card(y, 68.0, Some(('\u{E838}', None)), "Skins folder", &dir);
        let x = self.button(RIGHT, y + 34.0, "Open", Hit::OpenSkins);
        self.button(x - 8.0, y + 34.0, "Refresh", Hit::RefreshSkins);
        y += 72.0 + 20.0;

        // ---- library -----------------------------------------------------------------------
        let running = self.cfg.skins.iter().filter(|k| k.enabled).count();
        y = self.section(&format!("Library  ·  {running} on"), y);
        struct Row {
            depth: usize,
            dir: Option<(String, bool)>,
            name: String,
            rel: String,
        }
        fn flatten(nodes: &[crate::rainmeter::Node], depth: usize, expanded: &std::collections::HashSet<String>, out: &mut Vec<Row>) {
            for n in nodes {
                match n {
                    crate::rainmeter::Node::Dir { name, path, children } => {
                        let open = expanded.contains(path);
                        out.push(Row { depth, dir: Some((path.clone(), open)), name: name.clone(), rel: String::new() });
                        if open {
                            flatten(children, depth + 1, expanded, out);
                        }
                    }
                    crate::rainmeter::Node::Skin { name, rel } => out.push(Row { depth, dir: None, name: name.clone(), rel: rel.clone() }),
                }
            }
        }
        let mut rows = Vec::new();
        flatten(&self.tree, 0, &self.expanded, &mut rows);
        if rows.is_empty() {
            self.card(y, 60.0, Some(('\u{E783}', None)), "No skins found", "Install skins in Rainmeter's Skins folder, then press Refresh");
            y += 64.0;
        }
        for row in rows {
            let r = Rect::new(CONTENT_X, y, CONTENT_W, 40.0);
            let indent = row.depth as f32 * 22.0;
            match &row.dir {
                Some((path, open)) => {
                    let hit = Hit::Expand(path.clone());
                    let hov = self.hover.as_ref() == Some(&hit);
                    self.canvas.fill(r, 6.0, white(if hov { 0.07 } else { 0.04 }));
                    self.canvas.glyph(if *open { '\u{E70D}' } else { '\u{E76C}' }, Rect::new(r.x + 12.0 + indent, y, 18.0, 40.0), 11.0, white(0.7));
                    self.canvas.glyph('\u{E8B7}', Rect::new(r.x + 34.0 + indent, y, 22.0, 40.0), 15.0, rgba(0.94, 0.62, 0.15, 1.0));
                    self.canvas.text(&row.name, Rect::new(r.x + 64.0 + indent, y, CONTENT_W - 120.0, 40.0), Font::Text, 13.5, 500, white(0.92), Align::Left);
                    self.regions.push((r, hit));
                }
                None => {
                    let on = self.cfg.skin(&row.rel).is_some_and(|k| k.enabled);
                    let selected = on && self.selected.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&row.rel));
                    let select = Hit::SelectSkin(row.rel.clone());
                    let hov = self.hover.as_ref() == Some(&select);
                    self.canvas.fill(r, 6.0, white(if selected { 0.1 } else if hov { 0.07 } else { 0.035 }));
                    if selected {
                        self.canvas.fill(Rect::new(r.x, y + 10.0, 3.0, 20.0), 1.5, self.accent());
                    }
                    self.canvas.glyph('\u{E7C3}', Rect::new(r.x + 36.0 + indent, y, 22.0, 40.0), 14.0, white(if on { 0.95 } else { 0.5 }));
                    self.canvas.text(&row.name, Rect::new(r.x + 66.0 + indent, y, CONTENT_W - 190.0, 40.0), Font::Text, 13.5, 400, white(if on { 0.97 } else { 0.7 }), Align::Left);
                    if let Some((_, note)) = self.status.skin_notes.iter().find(|(k, _)| k.eq_ignore_ascii_case(&row.rel)) {
                        if !note.is_empty() && on {
                            let tag = Rect::new(RIGHT - 120.0, y + 11.0, 66.0, 18.0);
                            self.canvas.fill(tag, 9.0, rgba(0.98, 0.7, 0.35, 0.18));
                            self.canvas.text("Partial", tag, Font::Text, 10.5, 500, rgba(0.98, 0.76, 0.45, 1.0), Align::Center);
                        }
                    }
                    self.regions.push((r, select));
                    self.toggle(RIGHT, y + 20.0, on, Hit::SkinToggle(row.rel.clone()), false);
                }
            }
            y += 42.0;
        }
        y += 18.0;

        // ---- options of the selected skin -----------------------------------------------------------
        let Some(rel) = self.selected.clone().filter(|r| self.cfg.skin(r).is_some_and(|k| k.enabled)) else {
            return y;
        };
        let title = rel.rsplit('\\').next().unwrap_or(&rel).trim_end_matches(".ini").to_string();
        y = self.section(&format!("Options  ·  {title}"), y);
        let note = self.status.skin_notes.iter().find(|(k, _)| k.eq_ignore_ascii_case(&rel)).map(|(_, n)| n.clone()).unwrap_or_default();
        let (state, color) = if note.is_empty() {
            if self.status.skins_running == 0 { ("Starting…".to_string(), white(0.5)) } else { ("Fully supported".to_string(), rgba(0.42, 0.85, 0.55, 1.0)) }
        } else {
            (note, rgba(0.98, 0.76, 0.45, 1.0))
        };
        self.card(y, 68.0, Some(('\u{E73E}', None)), "Compatibility", "Lumen draws text and shapes, and time. More meters are coming");
        self.canvas.text(&state, Rect::new(RIGHT - 320.0, y, 320.0, 68.0), Font::Text, 12.5, 400, color, Align::Right);
        y += 72.0;

        self.card(y, 68.0, Some(('\u{E740}', None)), "Size", "Scales the whole skin");
        let v = self.slider_label(Slider::ClockSize);
        self.slider(RIGHT - 356.0, y + 34.0, 300.0, Slider::ClockSize, None);
        self.canvas.text(&v, Rect::new(RIGHT - 48.0, y + 16.0, 48.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        y += 72.0;
        self.card(y, 68.0, Some(('\u{E793}', None)), "Opacity", "How see-through the skin is");
        let v = self.slider_label(Slider::ClockOpacity);
        self.slider(RIGHT - 356.0, y + 34.0, 300.0, Slider::ClockOpacity, None);
        self.canvas.text(&v, Rect::new(RIGHT - 48.0, y + 16.0, 48.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        y += 72.0;
        self.card(y, 68.0, Some(('\u{E823}', None)), "Time format", "For skins that show the time");
        let sel = match self.sel().hour24 {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        self.segmented(RIGHT, y + 34.0, &[("Skin's setting", Hit::Hour(None)), ("12-hour", Hit::Hour(Some(false))), ("24-hour", Hit::Hour(Some(true)))], sel);
        y += 72.0;

        // Colour: skin default, swatches, then hue and saturation for anything else.
        self.card(y, 172.0, Some(('\u{E771}', None)), "Colour", "Text and lines");
        let current = theme::parse_hex(&self.sel().color);
        let mut x = CONTENT_X + 74.0;
        let cy = y + 84.0;
        self.swatch(x, cy, rgba(0.5, 0.5, 0.5, 1.0), current.is_none(), Hit::ClockColor(None));
        self.canvas.text("Skin", Rect::new(x - 20.0, cy + 14.0, 40.0, 16.0), Font::Text, 10.0, 400, white(0.6), Align::Center);
        x += 38.0;
        for &sw in SWATCHES {
            let c = theme::rgb(sw);
            let sel = current.is_some_and(|cur| theme::to_hex(cur) == theme::to_hex(c));
            self.swatch(x, cy, rgba(c[0], c[1], c[2], 1.0), sel, Hit::ClockColor(Some(sw)));
            x += 34.0;
        }
        let rail = Self::hue_rail();
        self.slider_row(y + 106.0, "Hue", Slider::ClockHue, Some(&rail));
        let (h, _, _) = theme::to_hsv(current.unwrap_or([1.0; 3]));
        let sats: Vec<Color> = (0..=4).map(|i| { let c = theme::hsv(h, i as f32 * 0.25, 1.0); rgba(c[0], c[1], c[2], 1.0) }).collect();
        self.slider_row(y + 134.0, "Saturation", Slider::ClockSat, Some(&sats));
        y += 176.0 + 20.0;

        let moving = self.status.moving_skin.eq_ignore_ascii_case(&rel);
        self.card(y, 68.0, Some(('\u{E7C2}', None)), "Move on the desktop", if moving { "Drag the skin on your desktop, then press Done" } else { "Lets you drag this skin to a new spot" });
        let x = self.button(RIGHT, y + 34.0, if moving { "Done" } else { "Move" }, Hit::MoveClock);
        self.button(x - 8.0, y + 34.0, "Reset all", Hit::ResetClock);
        y += 72.0;
        self.card(y, 68.0, Some(('\u{E72C}', None)), "Reload skins", "Edits to skin files also reload on their own within a minute");
        let x = self.button(RIGHT, y + 34.0, "Reload", Hit::ReloadSkin);
        if let Some(note) = self.notice.clone() {
            // A green check and the word, to the left of the button.
            self.canvas.glyph('\u{E73E}', Rect::new(x - 110.0, y + 22.0, 24.0, 24.0), 14.0, rgba(0.42, 0.85, 0.55, 1.0));
            self.canvas.text(&note, Rect::new(x - 84.0, y + 22.0, 76.0, 24.0), Font::Text, 13.0, 500, rgba(0.42, 0.85, 0.55, 1.0), Align::Left);
        }
        y + 68.0
    }

    fn page_launcher(&mut self, mut y: f32) -> f32 {
        let right = RIGHT;
        y = self.section("Launcher", y);
        self.card(y, 68.0, Some(('\u{E721}', None)), "Launcher", "Turn the launcher on or off");
        self.toggle(right, y + 34.0, self.cfg.modules.launcher, Hit::Module("launcher"), true);
        y += 72.0;

        let status = if self.recording {
            "Press the new shortcut, or Esc to cancel".to_string()
        } else if !self.cfg.modules.launcher {
            "Launcher is off".into()
        } else if self.status.hotkey_ok {
            "Active".into()
        } else {
            "In use by another app (PowerToys Run?). Lumen takes it as soon as it's free".into()
        };
        self.card(y, 76.0, Some(('\u{E765}', None)), "", "");
        let status_color = if self.recording {
            white(0.7)
        } else if self.status.hotkey_ok || !self.cfg.modules.launcher {
            rgba(0.42, 0.85, 0.55, 1.0)
        } else {
            rgba(0.98, 0.7, 0.35, 1.0)
        };
        self.canvas.text("Shortcut", Rect::new(CONTENT_X + 56.0, y + 14.0, 300.0, 22.0), Font::Text, 14.0, 400, white(0.95), Align::Left);
        self.canvas.text(&status, Rect::new(CONTENT_X + 56.0, y + 38.0, 420.0, 20.0), Font::Text, 12.0, 400, status_color, Align::Left);
        let keys: Vec<String> = if self.recording { vec!["…".into()] } else { self.cfg.launcher.hotkey.split('+').map(|s| s.trim().to_string()).collect() };
        let widths: Vec<f32> = keys.iter().map(|k| self.canvas.measure(k, Font::Text, 12.5, 600) + 20.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 6.0 * (keys.len() as f32 - 1.0) + 24.0;
        let field = Rect::new(right - total, y + 20.0, total, 36.0);
        let active = self.recording || self.hover == Some(Hit::Hotkey);
        self.canvas.fill(field, 6.0, white(if active { 0.08 } else { 0.04 }));
        self.canvas.stroke(field, 6.0, 1.0, if self.recording { self.accent() } else { white(0.1) });
        let mut kx = field.x + 12.0;
        for (k, w) in keys.iter().zip(&widths) {
            let cap = Rect::new(kx, y + 26.0, *w, 24.0);
            self.canvas.fill(cap, 4.0, white(0.1));
            self.canvas.text(k, cap, Font::Text, 12.5, 600, white(0.95), Align::Center);
            kx += w + 6.0;
        }
        self.regions.push((field, Hit::Hotkey));
        y += 80.0;

        self.card(y, 68.0, Some(('\u{E8FD}', None)), "Results shown", "How many rows the launcher shows at once");
        let plus = Rect::new(right - 32.0, y + 18.0, 32.0, 32.0);
        let minus = Rect::new(right - 32.0 - 44.0 - 32.0, y + 18.0, 32.0, 32.0);
        for (r, g, hit) in [(minus, '\u{E738}', Hit::MaxMinus), (plus, '\u{E710}', Hit::MaxPlus)] {
            let hov = self.hover.as_ref() == Some(&hit);
            self.canvas.fill(r, 5.0, white(if hov { 0.1 } else { 0.06 }));
            self.canvas.glyph(g, r, 11.0, white(0.9));
            self.regions.push((r, hit));
        }
        let n = self.cfg.launcher.max_results.to_string();
        self.canvas.text(&n, Rect::new(minus.x + 32.0, y, 44.0, 68.0), Font::Text, 15.0, 600, white(0.95), Align::Center);
        y += 72.0;

        self.card(y, 68.0, Some(('\u{E740}', None)), "Size", "Scales the whole launcher, text and icons");
        let v = self.slider_label(Slider::LauncherScale);
        self.slider(RIGHT - 356.0, y + 34.0, 300.0, Slider::LauncherScale, None);
        self.canvas.text(&v, Rect::new(RIGHT - 48.0, y + 16.0, 48.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        y += 72.0;

        self.card(y, 68.0, Some(('\u{E7AC}', None)), "Animation", "How the launcher appears and disappears");
        let styles: [(&'static str, &str); 5] = [("none", "None"), ("fade", "Fade"), ("slide_up", "Slide up"), ("slide_down", "Slide down"), ("zoom", "Zoom")];
        let opts: Vec<(&str, Hit)> = styles.iter().map(|(id, name)| (*name, Hit::Animation(id))).collect();
        let sel = styles.iter().position(|(id, _)| *id == self.cfg.launcher.animation).unwrap_or(1);
        self.segmented(right, y + 34.0, &opts, sel);
        y += 72.0;
        self.card(y, 68.0, Some(('\u{E916}', None)), "Animation speed", "Shorter is snappier; 0 turns the animation off");
        let v = self.slider_label(Slider::AnimSpeed);
        self.slider(RIGHT - 366.0, y + 34.0, 300.0, Slider::AnimSpeed, None);
        self.canvas.text(&v, Rect::new(RIGHT - 58.0, y + 16.0, 58.0, 36.0), Font::Text, 13.0, 500, white(0.9), Align::Right);
        y += 72.0;

        self.card(y, 68.0, Some(('\u{E774}', None)), "Search the web", "Last result for anything you type; opens your default browser");
        self.toggle(right, y + 34.0, self.cfg.launcher.web_fallback, Hit::WebFallback, true);
        y += 72.0;
        let engine_note = if self.cfg.launcher.search_engine == "custom" { "Using the custom address from your settings file" } else { "Which site the search opens" };
        self.card(y, 68.0, Some(('\u{E721}', None)), "Search engine", engine_note);
        let mut opts: Vec<(&str, Hit)> = crate::config::ENGINES.iter().map(|(id, name, _)| (*name, Hit::Engine(id))).collect();
        opts.truncate(4);
        let sel = crate::config::ENGINES.iter().position(|(id, _, _)| *id == self.cfg.launcher.search_engine).unwrap_or(usize::MAX);
        self.segmented(right, y + 34.0, &opts, sel);
        y += 72.0;

        self.card(y, 68.0, Some(('\u{E7FC}', None)), "Ignore shortcut in fullscreen", "Don't open over games and presentations");
        self.toggle(right, y + 34.0, self.cfg.launcher.ignore_in_fullscreen, Hit::Fullscreen, true);
        y += 72.0 + 20.0;

        y = self.section("Plugins", y);
        self.canvas.text("Keyword", Rect::new(right - 300.0, y - 30.0, 80.0, 24.0), Font::Text, 11.5, 400, white(0.5), Align::Center);
        self.canvas.text("Without keyword", Rect::new(right - 200.0, y - 30.0, 110.0, 24.0), Font::Text, 11.5, 400, white(0.5), Align::Center);
        self.canvas.text("On", Rect::new(right - 50.0, y - 30.0, 50.0, 24.0), Font::Text, 11.5, 400, white(0.5), Align::Center);
        for (id, _, _) in PLUGIN_DEFAULTS {
            let Some(p) = self.cfg.launcher.plugins.get(*id).cloned() else { continue };
            let (kind, name, desc) = plugin_info(id);
            self.card(y, 64.0, Some((kind.glyph(), Some(kind))), name, desc);
            let cy = y + 32.0;

            let kr = Rect::new(right - 290.0, cy - 15.0, 60.0, 30.0);
            let editing = self.editing.as_deref() == Some(*id);
            let hit = Hit::Keyword(id.to_string());
            let hov = self.hover.as_ref() == Some(&hit);
            self.canvas.fill(kr, 5.0, white(if editing { 0.1 } else if hov { 0.07 } else { 0.05 }));
            if editing {
                self.canvas.fill(Rect::new(kr.x + 1.0, kr.y + kr.h - 2.0, kr.w - 2.0, 2.0), 1.0, self.accent());
                let text = self.editor.text();
                let tw = self.canvas.measure(&text, Font::Text, 13.0, 400);
                let tx = kr.x + (kr.w - tw) / 2.0;
                if let Some((a, b)) = self.editor.selection() {
                    let xa = self.canvas.caret_x(&text, self.editor.utf16_index(a), Font::Text, 13.0);
                    let xb = self.canvas.caret_x(&text, self.editor.utf16_index(b), Font::Text, 13.0);
                    let ac = self.accent();
                    self.canvas.fill(Rect::new(tx + xa, kr.y + 6.0, xb - xa, kr.h - 12.0), 0.0, rgba(ac.r, ac.g, ac.b, 0.45));
                }
                self.canvas.text(&text, Rect::new(tx, kr.y, tw + 2.0, kr.h), Font::Text, 13.0, 400, white(0.95), Align::Left);
                if self.caret_on {
                    let cx = tx + self.canvas.caret_x(&text, self.editor.utf16_index(self.editor.caret), Font::Text, 13.0);
                    self.canvas.fill(Rect::new(cx, kr.y + 7.0, 1.2, kr.h - 14.0), 0.0, white(0.95));
                }
            } else {
                self.canvas.stroke(kr, 5.0, 1.0, white(0.08));
                let (t, col) = if p.keyword.is_empty() { ("none", white(0.35)) } else { (p.keyword.as_str(), white(0.95)) };
                self.canvas.text(t, kr, Font::Text, 13.0, 400, col, Align::Center);
            }
            self.regions.push((kr, hit));

            self.toggle(right - 125.0, cy, p.global, Hit::PluginGlobal(id.to_string()), false);
            self.toggle(right - 5.0, cy, p.enabled, Hit::PluginEnabled(id.to_string()), false);
            y += 68.0;
        }
        y
    }

    fn page_taskbar(&mut self, mut y: f32) -> f32 {
        y = self.section("Clear taskbar", y);
        let status = self.status.taskbar.clone();
        self.card(y, 68.0, Some(('\u{E7C4}', None)), "Clear taskbar", &status);
        self.toggle(RIGHT, y + 34.0, self.cfg.modules.taskbar, Hit::Module("taskbar"), true);
        y += 72.0;
        if self.status.translucenttb {
            self.card(y, 68.0, Some(('\u{E7BA}', None)), "TranslucentTB is running", "Both styling the taskbar at once fight each other");
            self.button(RIGHT, y + 34.0, "Quit TranslucentTB", Hit::QuitTtb);
            y += 72.0;
        }
        // Undo: always visible, so the taskbar can be put back no matter what.
        self.card(y, 68.0, Some(('\u{E7A7}', None)), "Restore normal taskbar", "Turns this off and puts back Windows' own look");
        let x = self.button(RIGHT, y + 34.0, "Restore", Hit::RestoreTaskbar);
        self.button(x - 8.0, y + 34.0, "Restart Explorer", Hit::RestartExplorer);
        y += 72.0;
        y += 20.0;
        y = self.section("Looks", y);
        let modes: Vec<(&'static str, &'static str)> = MODES.to_vec();
        for &(sit, title, desc) in SITUATIONS {
            let look = self.look(sit).clone();
            let detailed = look.mode == "tinted" || look.mode == "blur";
            let h = if detailed { 196.0 } else { 68.0 };
            self.card(y, h, Some(('\u{E7C4}', None)), title, desc);
            let opts: Vec<(&str, Hit)> = modes.iter().map(|(id, name)| (*name, Hit::TbMode(sit, id))).collect();
            let sel = modes.iter().position(|(id, _)| *id == look.mode).unwrap_or(0);
            self.segmented(RIGHT, y + 34.0, &opts, sel);
            if detailed {
                // Colour swatches, then opacity, hue and brightness sliders.
                let current = theme::parse_hex(&look.color).unwrap_or([0.0; 3]);
                let mut x = CONTENT_X + 74.0;
                let cy = y + 86.0;
                self.canvas.text("Colour", Rect::new(CONTENT_X + 56.0, cy - 30.0, 200.0, 20.0), Font::Text, 12.0, 400, white(0.6), Align::Left);
                for &sw in SWATCHES {
                    let c = theme::rgb(sw);
                    let selected = theme::to_hex(current) == theme::to_hex(c);
                    self.swatch(x, cy, rgba(c[0], c[1], c[2], 1.0), selected, Hit::TbColor(sit, sw));
                    x += 34.0;
                }
                self.slider_row(y + 106.0, "Opacity", Slider::TbOpacity(sit), None);
                let rail = Self::hue_rail();
                self.slider_row(y + 132.0, "Hue", Slider::TbHue(sit), Some(&rail));
                let (h, _, _) = theme::to_hsv(current);
                let lights: Vec<Color> = (0..=4).map(|i| { let c = theme::hsv(h, 0.65, i as f32 * 0.25); rgba(c[0], c[1], c[2], 1.0) }).collect();
                self.slider_row(y + 158.0, "Brightness", Slider::TbLight(sit), Some(&lights));
            }
            y += h + 4.0;
        }
        y + 4.0
    }

    fn page_about(&mut self, mut y: f32) -> f32 {
        y = self.section("Lumen", y);
        let version = format!("Version {}", env!("CARGO_PKG_VERSION"));
        self.card(y, 68.0, Some(('\u{E946}', None)), "Lumen", &version);
        y += 72.0;
        let apps = format!("{} apps indexed for the launcher", self.status.apps);
        self.card(y, 68.0, Some(('\u{E71D}', None)), "App index", &apps);
        y += 72.0;
        self.card(y, 68.0, Some(('\u{E8D7}', None)), "License", "GNU GPL v3 or later");
        y + 68.0
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        if !self.closed && !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

/// Launcher plugin kind, name and one-line description for the settings list.
pub fn plugin_info(id: &str) -> (Kind, &'static str, &'static str) {
    match id {
        "apps" => (Kind::App, "Apps", "Installed apps and desktop shortcuts"),
        "folder" => (Kind::Folder, "Folders", "Browse by typing a path like C:\\ or %appdata%"),
        "files" => (Kind::File, "Files", "Search files in your folders"),
        "calculator" => (Kind::Calculator, "Calculator", "Maths, with sqrt, sin, log, pi and more"),
        "units" => (Kind::Units, "Unit converter", "10 km to mi, 72 f in c, 1 gb mb"),
        "time" => (Kind::Time, "Time and date", "Time, date, week number, Unix time"),
        "values" => (Kind::Values, "Value generator", "GUIDs, hashes, Base64, URL encoding"),
        "windows" => (Kind::Window, "Window switcher", "Jump to any open window"),
        "shell" => (Kind::Shell, "Command Prompt", "Run a command"),
        "system" => (Kind::System, "System commands", "Lock, sleep, restart, shut down"),
        "settings" => (Kind::Settings, "Windows settings", "Open settings pages and control panels"),
        "services" => (Kind::Service, "Services", "Start, stop and restart services"),
        "registry" => (Kind::Registry, "Registry", "Browse keys and values"),
        "web" => (Kind::Web, "Web search", "Type ?? then your search to use the web directly"),
        _ => (Kind::App, "Plugin", ""),
    }
}

fn open(target: &str) {
    crate::launcher::shell::execute("open", target, "", None);
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: windows::Win32::Graphics::Gdi::CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x0020_2020)),
        lpszClassName: w!("LumenSettings"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("LumenSettings"),
        &HSTRING::from("Lumen settings"),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        960,
        680,
        None,
        None,
        instance,
        None,
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(r) = crate::app::settings_message(msg, wp, lp) {
        return r;
    }
    if msg == WM_CLOSE {
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// For `--settings-snapshot`: draws `page` into a bitmap without showing a window.
pub fn snapshot(cfg: &Config, status: Status, page: Page) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Graphics::Direct2D::Common::*;
    use windows::Win32::Graphics::Direct2D::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    let mut s = Settings::new(HWND::default(), cfg, status, 1.25, false);
    s.page = page;
    s.closed = true;
    s.refresh_library();
    let (w, h) = ((W * 1.25) as u32, (H * 1.25) as u32);
    unsafe {
        let dc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, bmp);
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 120.0,
            dpiY: 120.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let t = s.canvas.d2d().CreateDCRenderTarget(&props).ok()?;
        t.BindDC(dc, &RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }).ok()?;
        s.canvas.start(windows::core::Interface::cast(&t).ok()?).ok()?;
        s.draw();
        let _ = s.canvas.end();
        let _ = GdiFlush();
        let out = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(dc);
        Some((w, h, out))
    }
}
