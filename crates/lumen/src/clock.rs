//! Desktop clock module: renders a Rainmeter skin (Summit\Time by default) natively.
//!
//! The window is layered, click-through, never activates, stays at the bottom of the
//! z-order, and is redrawn only when the displayed text can change (once a minute for
//! Summit\Time), so it costs nothing between ticks.

use std::path::PathBuf;

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::ClockConfig;
use crate::gfx::{font_cache_dir, physical_size, Fonts, LayeredSurface};
use crate::rainmeter::skin::{Item, Overrides, Scene, Skin};
use crate::rainmeter::Install;

pub struct Clock {
    hwnd: HWND,
    skin: Option<Skin>,
    fonts: Fonts,
    surface: LayeredSurface,
    skins_root: Option<PathBuf>,
    rel: String,
    /// Position in skin (96-DPI) pixels, as Rainmeter stores it.
    pos: POINT,
    alpha: u8,
    /// Text of the last drawn frame; identical frames are not redrawn.
    last_key: String,
    shown: bool,
    cfg: ClockConfig,
    moving: bool,
    /// The last skin read failed; retried on every tick until it works.
    load_failed: bool,
}

impl Clock {
    pub fn create(cfg: &ClockConfig) -> Result<Self> {
        let install = Install::locate();
        let skins_root = if cfg.skins_path.is_empty() { install.skins.clone() } else { Some(PathBuf::from(&cfg.skins_path)) };
        let config_name = config_name(&cfg.skin);
        let saved = install.window_pos(&config_name);
        let pos = POINT { x: cfg.x.or(saved.map(|p| p.0)).unwrap_or(100), y: cfg.y.or(saved.map(|p| p.1)).unwrap_or(100) };
        let alpha = rainmeter_alpha(&install, &config_name);

        let hwnd = unsafe { create_window(pos)? };
        let mut clock = Self {
            hwnd,
            skin: None,
            fonts: Fonts::new()?,
            surface: LayeredSurface::new()?,
            skins_root,
            rel: cfg.skin.clone(),
            pos,
            alpha,
            last_key: String::new(),
            shown: false,
            cfg: cfg.clone(),
            moving: false,
            load_failed: false,
        };
        clock.reload();
        Ok(clock)
    }

    /// (Re)reads the skin and its fonts from disk. Failures leave the clock hidden and logged.
    pub fn reload(&mut self) {
        self.last_key.clear();
        let Some(root) = &self.skins_root else {
            crate::error!("Rainmeter Skins folder not found; clock disabled until it exists");
            self.skin = None;
            return;
        };
        match Skin::load(root, &self.rel, &overrides(&self.cfg)) {
            Ok(skin) => {
                let root_name = self.rel.split(['\\', '/']).next().unwrap_or("skin").to_string();
                self.fonts.load_dir(&skin.fonts_dir, &font_cache_dir(&root_name));
                crate::info!("loaded skin {}", skin.file.display());
                self.skin = Some(skin);
            }
            Err(e) => {
                // A failed read (OneDrive syncing the file, an editor mid-save) must not take
                // the clock away: keep showing the last good skin and try again on the next tick.
                if !self.load_failed {
                    crate::warn!("could not read the skin, will keep retrying: {e}");
                }
                self.load_failed = true;
                return;
            }
        }
        self.load_failed = false;
    }

    pub fn scene(&mut self) -> Option<Scene> {
        let skin = self.skin.as_ref()?;
        let now = unsafe { GetLocalTime() };
        let fonts = &mut self.fonts;
        let mut scene = skin.layout(&now, &mut |text, style| fonts.measure(text, style));
        if self.moving {
            // A visible frame while dragging, so the clock is easy to grab.
            let (w, h) = (scene.width as f32, scene.height as f32);
            scene.items.insert(0, Item::Rect { x: 1.0, y: 1.0, w: w - 2.0, h: h - 2.0, radius: 10.0, fill: [1.0, 1.0, 1.0, 0.10], stroke: [1.0, 1.0, 1.0, 0.6], stroke_width: 1.5 });
        }
        Some(scene)
    }

    /// New settings from the settings window. Returns true if the skin must be re-read.
    pub fn apply_config(&mut self, cfg: &ClockConfig) {
        let reread = overrides(cfg) != overrides(&self.cfg);
        let moved = cfg.x != self.cfg.x || cfg.y != self.cfg.y;
        self.cfg = cfg.clone();
        if moved {
            if let (Some(x), Some(y)) = (cfg.x, cfg.y) {
                self.pos = POINT { x, y };
            } else {
                let install = Install::locate();
                if let Some((x, y)) = install.window_pos(&config_name(&self.rel)) {
                    self.pos = POINT { x, y };
                }
            }
        }
        if reread {
            self.reload();
        }
        self.last_key.clear();
    }

    /// Move mode: the clock takes mouse input so it can be dragged.
    pub fn set_moving(&mut self, on: bool) {
        self.moving = on;
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            let ex = if on { ex & !WS_EX_TRANSPARENT.0 } else { ex | WS_EX_TRANSPARENT.0 };
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex as isize);
        }
        self.set_flag(FLAG_MOVING, on);
        self.last_key.clear();
    }

    /// Show Desktop (Win+D) or a click on the wallpaper brings the desktop to the front,
    /// which would cover a bottom-most window. Like Rainmeter's "On desktop" skins, the
    /// clock rises above everything while the desktop is in front (without taking focus)
    /// and drops back behind windows as soon as one is used.
    pub fn set_desktop_in_front(&mut self, on: bool) {
        if self.flag(FLAG_RAISED) == on || !self.shown {
            return;
        }
        self.set_flag(FLAG_RAISED, on);
        unsafe {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
            if on {
                let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            } else {
                let _ = SetWindowPos(self.hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
                let _ = SetWindowPos(self.hwnd, HWND_BOTTOM, 0, 0, 0, 0, flags);
            }
        }
    }

    fn flag(&self, f: isize) -> bool {
        unsafe { GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) & f != 0 }
    }

    fn set_flag(&self, f: isize, on: bool) {
        unsafe {
            let v = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA);
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, if on { v | f } else { v & !f });
        }
    }

    pub fn moving(&self) -> bool {
        self.moving
    }

    /// After a drag: reads where the window ended up, in skin pixels.
    pub fn read_position(&mut self) -> (i32, i32) {
        let mut r = windows::Win32::Foundation::RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        let scale = display_scale();
        self.pos = POINT { x: (r.left as f32 / scale).round() as i32, y: (r.top as f32 / scale).round() as i32 };
        (self.pos.x, self.pos.y)
    }

    /// Redraws if the visible text changed (or `force`). Returns ms until the next tick.
    pub fn tick(&mut self, force: bool) -> u32 {
        if self.skin.as_ref().is_some_and(|s| s.changed_on_disk()) {
            crate::info!("skin changed on disk, reloading");
            self.reload();
        } else if (self.skin.is_none() && self.skins_root.is_some()) || self.load_failed {
            self.reload();
        }
        let Some(scene) = self.scene() else {
            self.hide();
            return 5_000;
        };
        let display = display_scale();
        let scale = display * self.cfg.size.clamp(0.3, 4.0);
        let alpha = (self.alpha as f32 * self.cfg.opacity.clamp(0.1, 1.0)).round() as u8;
        let key = format!("{scale}|{alpha}|{}|{}", self.moving, scene_key(&scene));
        if force || key != self.last_key {
            let screen = self.keep_on_screen(&scene, scale, display);
            match self.surface.present(self.hwnd, &mut self.fonts, &scene, scale, screen, alpha) {
                Ok(()) => {
                    self.last_key = key;
                    self.show();
                }
                Err(e) => {
                    crate::error!("clock render failed: {e}");
                    self.last_key.clear();
                    return 5_000;
                }
            }
        }
        self.next_delay()
    }

    pub fn has_skin(&self) -> bool {
        self.skin.is_some()
    }

    pub fn needs_seconds(&self) -> bool {
        self.skin.as_ref().is_some_and(|s| s.needs_seconds)
    }

    fn next_delay(&self) -> u32 {
        let now = unsafe { GetLocalTime() };
        let ms_into_second = now.wMilliseconds as u32;
        // Land ~40 ms after the boundary so the new minute is definitely visible.
        if self.needs_seconds() {
            1000 - ms_into_second + 40
        } else {
            (59 - now.wSecond as u32) * 1000 + (1000 - ms_into_second) + 40
        }
    }

    /// Screen position for the current scale. Like Rainmeter's KeepOnScreen, a clock that
    /// would land on no monitor (e.g. one was unplugged) is moved onto the primary one.
    fn keep_on_screen(&mut self, scene: &Scene, scale: f32, display: f32) -> POINT {
        let (w, h) = physical_size(scene, scale);
        let screen = POINT { x: (self.pos.x as f32 * display).round() as i32, y: (self.pos.y as f32 * display).round() as i32 };
        unsafe {
            let center = POINT { x: screen.x + w as i32 / 2, y: screen.y + h as i32 / 2 };
            if !MonitorFromPoint(center, MONITOR_DEFAULTTONULL).is_invalid() {
                return screen;
            }
            let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(mon, &mut info).as_bool() {
                return screen;
            }
            let wa = info.rcWork;
            let x = screen.x.clamp(wa.left, (wa.right - w as i32).max(wa.left));
            let y = screen.y.clamp(wa.top, (wa.bottom - h as i32).max(wa.top));
            crate::warn!("clock was off-screen, moved to {x},{y}");
            self.pos = POINT { x: (x as f32 / display) as i32, y: (y as f32 / display) as i32 };
            POINT { x, y }
        }
    }

    fn show(&mut self) {
        unsafe {
            if !self.shown {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.shown = true;
            }
            if !self.flag(FLAG_RAISED) {
                let _ = SetWindowPos(self.hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
            }
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
    }

    /// For `--snapshot`: the current frame as premultiplied BGRA.
    pub fn snapshot(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let scene = self.scene()?;
        self.surface.render_pixels(&mut self.fonts, &scene, display_scale()).ok()
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn overrides(cfg: &ClockConfig) -> Overrides {
    let color = crate::theme::parse_hex(&cfg.color).map(|c| {
        let b = |v: f32| (v * 255.0).round() as u8;
        format!("{},{},{}", b(c[0]), b(c[1]), b(c[2]))
    });
    Overrides { hour24: cfg.hour24, color }
}

/// Rainmeter scales skins by the primary display's scaling (125% → 1.25).
fn display_scale() -> f32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x >= 48 {
            x as f32 / 96.0
        } else {
            1.0
        }
    }
}

/// `Summit\Time\Time.ini` → `Summit\Time` (the section name in Rainmeter.ini).
fn config_name(rel: &str) -> String {
    let rel = rel.replace('/', "\\");
    match rel.rfind('\\') {
        Some(i) => rel[..i].to_string(),
        None => rel,
    }
}

fn rainmeter_alpha(install: &Install, config: &str) -> u8 {
    crate::rainmeter::ini::Ini::load(&install.settings)
        .ok()
        .and_then(|ini| ini.section(config).and_then(|s| s.get("AlphaValue")).and_then(|v| v.trim().parse::<u8>().ok()))
        .unwrap_or(255)
}

fn scene_key(scene: &Scene) -> String {
    use crate::rainmeter::skin::Item;
    let mut key = format!("{}x{}", scene.width, scene.height);
    for item in &scene.items {
        if let Item::Text { text, .. } = item {
            key.push('|');
            key.push_str(text);
        }
    }
    key
}

unsafe fn create_window(pos: POINT) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("LumenClock"),
        ..Default::default()
    };
    // Registering twice (module toggled off and on) fails harmlessly.
    RegisterClassExW(&class);
    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    CreateWindowExW(ex, w!("LumenClock"), w!("Lumen Clock"), WS_POPUP, pos.x, pos.y, 1, 1, None, None, instance, None)
}

/// Window flags kept in GWLP_USERDATA, so the window procedure needs no module state.
const FLAG_MOVING: isize = 1;
const FLAG_RAISED: isize = 2;

/// Stateless on purpose: it never touches module state, so it is safe to re-enter from
/// SetWindowPos/UpdateLayeredWindow calls made while rendering.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let flags = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    match msg {
        // Pinned to the bottom of the z-order like a desktop widget, unless the desktop is
        // in front (see set_desktop_in_front).
        WM_WINDOWPOSCHANGING if flags & FLAG_RAISED == 0 => {
            let wpos = lp.0 as *mut WINDOWPOS;
            if !wpos.is_null() {
                (*wpos).hwndInsertAfter = HWND_BOTTOM;
                (*wpos).flags &= !SWP_NOZORDER;
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // In move mode the whole clock is a drag handle.
        WM_NCHITTEST if flags & FLAG_MOVING != 0 => LRESULT(HTCAPTION as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_EXITSIZEMOVE => {
            if let Ok(app) = FindWindowW(w!("LumenApp"), None) {
                let _ = PostMessageW(app, crate::app::WM_CLOCK_MOVED, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
