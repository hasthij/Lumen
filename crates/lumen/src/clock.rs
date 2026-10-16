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
use crate::rainmeter::skin::{Scene, Skin};
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
        match Skin::load(root, &self.rel) {
            Ok(skin) => {
                let root_name = self.rel.split(['\\', '/']).next().unwrap_or("skin").to_string();
                self.fonts.load_dir(&skin.fonts_dir, &font_cache_dir(&root_name));
                crate::info!("loaded skin {}", skin.file.display());
                self.skin = Some(skin);
            }
            Err(e) => {
                crate::error!("{e}");
                self.skin = None;
            }
        }
    }

    pub fn scene(&mut self) -> Option<Scene> {
        let skin = self.skin.as_ref()?;
        let now = unsafe { GetLocalTime() };
        let fonts = &mut self.fonts;
        Some(skin.layout(&now, &mut |text, style| fonts.measure(text, style)))
    }

    /// Redraws if the visible text changed (or `force`). Returns ms until the next tick.
    pub fn tick(&mut self, force: bool) -> u32 {
        if self.skin.as_ref().is_some_and(|s| s.changed_on_disk()) {
            crate::info!("skin changed on disk, reloading");
            self.reload();
        }
        let Some(scene) = self.scene() else {
            self.hide();
            return 30_000;
        };
        let scale = display_scale();
        let key = format!("{scale}|{}", scene_key(&scene));
        if force || key != self.last_key {
            let screen = self.keep_on_screen(&scene, scale);
            match self.surface.present(self.hwnd, &mut self.fonts, &scene, scale, screen, self.alpha) {
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
    fn keep_on_screen(&mut self, scene: &Scene, scale: f32) -> POINT {
        let (w, h) = physical_size(scene, scale);
        let screen = POINT { x: (self.pos.x as f32 * scale).round() as i32, y: (self.pos.y as f32 * scale).round() as i32 };
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
            self.pos = POINT { x: (x as f32 / scale) as i32, y: (y as f32 / scale) as i32 };
            POINT { x, y }
        }
    }

    fn show(&mut self) {
        unsafe {
            if !self.shown {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.shown = true;
            }
            let _ = SetWindowPos(self.hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
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

/// Stateless on purpose: it never touches module state, so it is safe to re-enter from
/// SetWindowPos/UpdateLayeredWindow calls made while rendering.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            // Pin to the bottom of the z-order, like a desktop widget.
            let wpos = lp.0 as *mut WINDOWPOS;
            if !wpos.is_null() {
                (*wpos).hwndInsertAfter = HWND_BOTTOM;
                (*wpos).flags &= !SWP_NOZORDER;
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
