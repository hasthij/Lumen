//! The dock: two windows (an acrylic "shelf" and a Direct2D "icons" layer above it), the item
//! list, layout, slide animation and input.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use windows::core::{s, w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{self, Config, Pin};
use crate::gfx::Gfx;
use crate::hooks::{self, MOUSE_AT_EDGE, MOUSE_IN_DOCK, WM_APP_CONFIG, WM_APP_MOUSE, WM_APP_REFRESH};
use crate::shell;
use crate::winlist::ExeCache;

const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_REVEAL: usize = 1;
const TIMER_HIDE: usize = 2;
const SLIDE_MS: f32 = 220.0;

#[derive(Clone, PartialEq)]
struct Item {
    name: String,
    /// Icon source tried first (a path or shell:AppsFolder item), then `icon_alt`.
    icon: String,
    icon_alt: Option<String>,
    pin: Option<usize>,
    /// The app's windows in z-order, topmost first.
    windows: Vec<HWND>,
    active: bool,
    /// Image path of a running app (for unpinned items and "Open file location").
    exe: Option<String>,
}

struct PinMatch {
    exe: Option<String>,
    aumid: Option<String>,
}

#[derive(Default)]
struct Layout {
    scale: f32,
    icon: f32,
    pad_top: f32,
    slots: Vec<f32>,
    sep_x: Option<f32>,
    x: i32,
    w: i32,
    h: i32,
    y_shown: i32,
    y_hidden: i32,
}

enum Deferred {
    Launch(String, String),
    Menu(Option<usize>),
}

pub struct Dock {
    pub icons_hwnd: HWND,
    shelf_hwnd: HWND,
    gfx: Gfx,
    wic: IWICImagingFactory,
    brush: ID2D1SolidColorBrush,
    bitmaps: HashMap<String, Option<ID2D1Bitmap1>>,
    cfg: Config,
    matches: Vec<PinMatch>,
    items: Vec<Item>,
    extra_order: Vec<String>,
    exes: ExeCache,
    tray: HWND,
    taskbar_created: u32,

    monitor: HMONITOR,
    mon: RECT,
    work: RECT,
    layout: Layout,

    covered: bool,
    fullscreen: bool,
    hover_reveal: bool,
    menu_open: bool,
    at_edge: bool,
    in_dock: bool,

    /// 0 = hidden, 1 = fully shown.
    pos: f32,
    anim_from: f32,
    anim_to: f32,
    anim_start: Option<Instant>,
    on_screen: bool,

    hot: Option<usize>,
    pressed: Option<usize>,
    tracking_leave: bool,
    deferred: Vec<Deferred>,
}

thread_local! {
    pub static DOCK: RefCell<Option<Dock>> = const { RefCell::new(None) };
}

// ---- window plumbing ------------------------------------------------------------------------

unsafe extern "system" fn shelf_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // Black is fully transparent under the acrylic accent.
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            FillRect(hdc, &ps.rcPaint, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn icons_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    let result = DOCK.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(|dock| dock.handle(msg, wp, lp)),
        Err(_) => {
            // Re-entered while the dock is busy: keep our own events instead of dropping them.
            if matches!(msg, WM_APP_REFRESH | WM_APP_MOUSE | WM_APP_CONFIG | WM_TIMER) {
                let _ = PostMessageW(hwnd, msg, wp, lp);
                return Some(Some(LRESULT(0)));
            }
            None
        }
    });
    run_deferred();
    match result {
        Some(Some(r)) => r,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Work that can pump messages (ShellExecute, popup menus) runs here, outside the RefCell borrow.
fn run_deferred() {
    loop {
        let job = DOCK.with(|cell| cell.try_borrow_mut().ok().and_then(|mut g| g.as_mut().and_then(|d| d.deferred.pop())));
        match job {
            Some(Deferred::Launch(target, args)) => shell::launch(&target, &args),
            Some(Deferred::Menu(index)) => show_menu(index),
            None => break,
        }
    }
}

fn register_class(name: PCWSTR, proc: WNDPROC) {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: proc,
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// Undocumented, but it is the only acrylic that stays on for a window that never activates
/// (DWMWA_SYSTEMBACKDROP_TYPE falls back to a flat colour when inactive).
fn set_acrylic(hwnd: HWND, abgr: u32) {
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        color: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionData {
        attribute: u32,
        data: *mut std::ffi::c_void,
        size: usize,
    }
    type SetWca = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
    const WCA_ACCENT_POLICY: u32 = 19;
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else { return };
        let f: SetWca = std::mem::transmute(f);
        let mut accent = AccentPolicy { state: ACCENT_ENABLE_ACRYLICBLURBEHIND, flags: 2, color: abgr, animation: 0 };
        let mut data = CompositionData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut accent as *mut _ as *mut _,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        let _ = f(hwnd, &mut data);
    }
}

fn set_dwm<T>(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: T) {
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, attr, &value as *const T as *const _, std::mem::size_of::<T>() as u32);
    }
}

fn find_tray() -> HWND {
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.unwrap_or_default()
}

fn pin_match(pin: &Pin) -> PinMatch {
    let target = shell::expand_env(&pin.target);
    let lower = target.to_lowercase();
    let aumid = lower.strip_prefix(&shell::APPS_FOLDER.to_lowercase()).map(str::to_string);
    let exe = pin.exe.as_ref().map(|e| e.to_lowercase()).or_else(|| {
        if lower.ends_with(".exe") {
            Some(shell::file_name_lower(&target))
        } else if lower.ends_with(".lnk") {
            shell::resolve_lnk(&target).map(|t| shell::file_name_lower(&t))
        } else {
            None
        }
    });
    PinMatch { exe, aumid }
}

fn exe_stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name).to_string()
}

impl Dock {
    pub fn create() -> Result<HWND> {
        let shelf_class = w!("LumenDockShelf");
        let icons_class = w!("LumenDockIcons");
        register_class(shelf_class, Some(shelf_proc));
        register_class(icons_class, Some(icons_proc));
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let shelf = CreateWindowExW(ex, shelf_class, w!("Lumen Dock"), WS_POPUP, 0, 0, 1, 1, None, None, instance, None)?;
            // Owned by the shelf, so it always stays directly above it.
            let icons = CreateWindowExW(
                ex | WS_EX_NOREDIRECTIONBITMAP,
                icons_class,
                w!("Lumen Dock"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                shelf,
                None,
                instance,
                None,
            )?;

            set_dwm(shelf, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
            set_dwm(shelf, DWMWA_BORDER_COLOR, 0x0050_5050u32);
            set_dwm(shelf, DWMWA_USE_IMMERSIVE_DARK_MODE, BOOL(1));
            set_acrylic(shelf, 0x6620_2020);

            let gfx = Gfx::new(icons, 1, 1)?;
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let brush = gfx.dc.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, None)?;

            let cfg = config::load();
            let matches = cfg.pins.iter().map(pin_match).collect();
            let dock = Dock {
                icons_hwnd: icons,
                shelf_hwnd: shelf,
                gfx,
                wic,
                brush,
                bitmaps: HashMap::new(),
                cfg,
                matches,
                items: Vec::new(),
                extra_order: Vec::new(),
                exes: ExeCache::default(),
                tray: find_tray(),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                monitor: HMONITOR::default(),
                mon: RECT::default(),
                work: RECT::default(),
                layout: Layout::default(),
                covered: false,
                fullscreen: false,
                hover_reveal: false,
                menu_open: false,
                at_edge: false,
                in_dock: false,
                pos: 0.0,
                anim_from: 0.0,
                anim_to: 0.0,
                anim_start: None,
                on_screen: false,
                hot: None,
                pressed: None,
                tracking_leave: false,
                deferred: Vec::new(),
            };
            DOCK.with(|cell| *cell.borrow_mut() = Some(dock));
            DOCK.with(|cell| {
                let mut guard = cell.borrow_mut();
                let dock = guard.as_mut().unwrap();
                dock.refresh(false);
                dock.relayout();
            });
            Ok(icons)
        }
    }

    pub fn is_animating(&self) -> bool {
        self.anim_start.is_some()
    }

    // ---- state ------------------------------------------------------------------------------

    fn refresh(&mut self, redraw: bool) {
        let own = [self.shelf_hwnd, self.icons_hwnd];
        if self.monitor.is_invalid() {
            self.update_monitor();
        }
        let snap = self.exes.snapshot(&own, self.monitor, self.mon);
        self.covered = snap.covered;
        self.fullscreen = snap.fullscreen;

        let mut items: Vec<Item> = self
            .cfg
            .pins
            .iter()
            .enumerate()
            .map(|(i, p)| Item {
                name: p.name.clone(),
                icon: shell::expand_env(&p.target),
                icon_alt: None,
                pin: Some(i),
                windows: Vec::new(),
                active: false,
                exe: None,
            })
            .collect();
        let mut extras: Vec<(String, Item)> = Vec::new();

        for w in snap.windows {
            let exe_name = shell::file_name_lower(&w.exe);
            let aumid = w.aumid.as_ref().map(|a| a.to_lowercase());
            let pinned = self.matches.iter().position(|m| {
                m.exe.as_deref() == Some(exe_name.as_str()) || (m.aumid.is_some() && m.aumid == aumid)
            });
            if let Some(i) = pinned {
                items[i].windows.push(w.hwnd);
                items[i].exe.get_or_insert(w.exe);
                continue;
            }
            if !self.cfg.show_running {
                continue;
            }
            let key = w.exe.to_lowercase();
            match extras.iter_mut().find(|(k, _)| *k == key) {
                Some((_, item)) => item.windows.push(w.hwnd),
                None => {
                    let icon = match &w.aumid {
                        Some(a) => format!("{}{a}", shell::APPS_FOLDER),
                        None => w.exe.clone(),
                    };
                    let item = Item {
                        name: exe_stem(&w.exe),
                        icon_alt: w.aumid.is_some().then(|| w.exe.clone()),
                        icon,
                        pin: None,
                        windows: vec![w.hwnd],
                        active: false,
                        exe: Some(w.exe.clone()),
                    };
                    extras.push((key, item));
                }
            }
        }

        // Unpinned apps keep the order they first appeared in, like the macOS dock.
        self.extra_order.retain(|k| extras.iter().any(|(e, _)| e == k));
        for (k, _) in &extras {
            if !self.extra_order.contains(k) {
                self.extra_order.push(k.clone());
            }
        }
        extras.sort_by_key(|(k, _)| self.extra_order.iter().position(|o| o == k));
        items.extend(extras.into_iter().map(|(_, item)| item));
        for item in &mut items {
            item.active = item.windows.contains(&snap.foreground);
        }

        let count_changed = items.len() != self.items.len()
            || items.iter().zip(&self.items).any(|(a, b)| a.pin.is_some() != b.pin.is_some());
        let changed = items != self.items;
        self.items = items;
        if redraw {
            if count_changed {
                self.relayout();
            } else if changed {
                self.draw();
            }
        }
        self.update_target();
    }

    fn update_monitor(&mut self) {
        unsafe {
            self.monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(self.monitor, &mut info);
            self.mon = info.rcMonitor;
            self.work = info.rcWork;
        }
    }

    fn relayout(&mut self) {
        self.update_monitor();
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        unsafe {
            let _ = GetDpiForMonitor(self.monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        }
        let scale = dpi_x as f32 / 96.0;
        if scale != self.layout.scale {
            self.bitmaps.clear();
        }

        let icon = (self.cfg.icon_size * scale).round();
        let gap = (8.0 * scale).round();
        let pad_x = (10.0 * scale).round();
        let pad_top = (9.0 * scale).round();
        let bottom = (12.0 * scale).round();
        let pinned = self.items.iter().filter(|i| i.pin.is_some()).count();
        let has_sep = pinned > 0 && pinned < self.items.len();

        let mut slots = Vec::with_capacity(self.items.len());
        let mut sep_x = None;
        let mut x = pad_x;
        for i in 0..self.items.len() {
            if has_sep && i == pinned {
                sep_x = Some(x + (scale / 2.0).round());
                x += scale.round().max(1.0) + gap;
            }
            slots.push(x);
            x += icon + gap;
        }
        let content = if self.items.is_empty() { icon } else { x - gap - pad_x };
        let w = (content + 2.0 * pad_x) as i32;
        let h = (pad_top + icon + bottom) as i32;
        let mon_w = self.mon.right - self.mon.left;
        let margin = (self.cfg.margin * scale).round() as i32;

        self.layout = Layout {
            scale,
            icon,
            pad_top,
            slots,
            sep_x,
            x: self.mon.left + (mon_w - w) / 2,
            w,
            h,
            y_shown: self.work.bottom - margin - h,
            y_hidden: self.mon.bottom,
        };
        let l = &self.layout;
        hooks::ZONE.set((self.mon.left, self.mon.right, self.mon.bottom), (l.x, l.y_shown, l.x + l.w, l.y_shown + l.h));
        let _ = self.gfx.resize(l.w as u32, l.h as u32);
        self.place(true);
        self.draw();
    }

    fn want_visible(&self) -> bool {
        if self.menu_open {
            return true;
        }
        if self.fullscreen {
            return false;
        }
        !(self.cfg.hide_when_maximized && self.covered) || self.hover_reveal
    }

    fn update_target(&mut self) {
        let to = if self.want_visible() { 1.0 } else { 0.0 };
        if to != self.anim_to {
            self.anim_from = self.pos;
            self.anim_to = to;
            self.anim_start = Some(Instant::now());
        }
    }

    /// Advances the slide animation by one frame. Returns whether it is still running.
    pub fn tick(&mut self) -> bool {
        let Some(start) = self.anim_start else { return false };
        let span = (self.anim_to - self.anim_from).abs().max(0.01);
        let t = (start.elapsed().as_secs_f32() * 1000.0 / (SLIDE_MS * span)).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.pos = self.anim_from + (self.anim_to - self.anim_from) * eased;
        if t >= 1.0 {
            self.pos = self.anim_to;
            self.anim_start = None;
        }
        self.place(false);
        self.anim_start.is_some()
    }

    fn place(&mut self, force_z: bool) {
        let l = &self.layout;
        if self.pos <= 0.0 {
            if self.on_screen {
                unsafe {
                    let _ = ShowWindow(self.icons_hwnd, SW_HIDE);
                    let _ = ShowWindow(self.shelf_hwnd, SW_HIDE);
                }
                self.on_screen = false;
            }
            return;
        }
        let y = (l.y_hidden as f32 + (l.y_shown - l.y_hidden) as f32 * self.pos).round() as i32;
        let appearing = !self.on_screen;
        unsafe {
            if appearing || force_z {
                // Sit just under the native taskbar so the dock slides behind it when hiding.
                if !IsWindow(self.tray).as_bool() {
                    self.tray = find_tray();
                }
                let after = if self.tray.is_invalid() { HWND_TOPMOST } else { self.tray };
                let flags = SWP_NOACTIVATE;
                let _ = SetWindowPos(self.icons_hwnd, after, l.x, y, l.w, l.h, flags);
                let _ = SetWindowPos(self.shelf_hwnd, self.icons_hwnd, l.x, y, l.w, l.h, flags);
            } else {
                let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE;
                let hdwp = BeginDeferWindowPos(2).unwrap_or_default();
                let hdwp = DeferWindowPos(hdwp, self.shelf_hwnd, None, l.x, y, 0, 0, flags).unwrap_or_default();
                let hdwp = DeferWindowPos(hdwp, self.icons_hwnd, None, l.x, y, 0, 0, flags).unwrap_or_default();
                let _ = EndDeferWindowPos(hdwp);
            }
            if appearing {
                let _ = ShowWindow(self.shelf_hwnd, SW_SHOWNOACTIVATE);
                let _ = ShowWindow(self.icons_hwnd, SW_SHOWNOACTIVATE);
                self.on_screen = true;
            }
        }
    }

    // ---- drawing ----------------------------------------------------------------------------

    fn bitmap(&mut self, index: usize) -> Option<ID2D1Bitmap1> {
        let px = (self.layout.icon * 2.0).min(256.0) as i32;
        let item = &self.items[index];
        for source in std::iter::once(&item.icon).chain(item.icon_alt.as_ref()) {
            if !self.bitmaps.contains_key(source) {
                let bmp = shell::load_icon(&self.gfx.dc, &self.wic, source, px);
                self.bitmaps.insert(source.clone(), bmp);
            }
            if let Some(Some(bmp)) = self.bitmaps.get(source) {
                return Some(bmp.clone());
            }
        }
        None
    }

    fn draw(&mut self) {
        let bitmaps: Vec<_> = (0..self.items.len()).map(|i| self.bitmap(i)).collect();
        let l = &self.layout;
        let s = l.scale;
        if self.gfx.begin().is_err() {
            return;
        }
        let dc = &self.gfx.dc;
        let color = |a: f32| D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a };
        unsafe {
            for (i, item) in self.items.iter().enumerate() {
                let x = l.slots[i];
                let rect = D2D_RECT_F { left: x, top: l.pad_top, right: x + l.icon, bottom: l.pad_top + l.icon };
                let opacity = if self.pressed == Some(i) && self.hot == Some(i) { 0.6 } else { 1.0 };
                match &bitmaps[i] {
                    Some(bmp) => dc.DrawBitmap(bmp, Some(&rect), opacity, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None),
                    None => {
                        self.brush.SetColor(&color(0.25 * opacity));
                        let rr = D2D1_ROUNDED_RECT { rect, radiusX: 10.0 * s, radiusY: 10.0 * s };
                        dc.FillRoundedRectangle(&rr, &self.brush);
                    }
                }
                if !item.windows.is_empty() {
                    self.brush.SetColor(&color(if item.active { 1.0 } else { 0.6 }));
                    let r = (2.0 * s).max(1.5);
                    let point = D2D_POINT_2F { x: x + l.icon / 2.0, y: l.pad_top + l.icon + 6.0 * s };
                    dc.FillEllipse(&D2D1_ELLIPSE { point, radiusX: r, radiusY: r }, &self.brush);
                }
            }
            if let Some(sx) = l.sep_x {
                self.brush.SetColor(&color(0.28));
                let rect = D2D_RECT_F {
                    left: sx,
                    top: l.pad_top + l.icon * 0.12,
                    right: sx + s.round().max(1.0),
                    bottom: l.pad_top + l.icon * 0.88,
                };
                dc.FillRectangle(&rect, &self.brush);
            }
        }
        if self.gfx.end().is_err() {
            // Device lost (driver update, GPU reset): rebuild everything on the next relayout.
            if let Ok(gfx) = Gfx::new(self.icons_hwnd, self.layout.w as u32, self.layout.h as u32) {
                if let Ok(brush) = unsafe { gfx.dc.CreateSolidColorBrush(&color(1.0), None) } {
                    self.gfx = gfx;
                    self.brush = brush;
                    self.bitmaps.clear();
                }
            }
        }
    }

    // ---- input ------------------------------------------------------------------------------

    fn hit(&self, lp: LPARAM) -> Option<usize> {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        let l = &self.layout;
        if y < l.pad_top - 6.0 * l.scale || y > l.pad_top + l.icon + 10.0 * l.scale {
            return None;
        }
        let half_gap = 4.0 * l.scale;
        l.slots.iter().position(|&sx| x >= sx - half_gap && x < sx + l.icon + half_gap)
    }

    fn activate(&mut self, index: usize) {
        let item = &self.items[index];
        if item.windows.is_empty() {
            self.launch(index);
            return;
        }
        unsafe {
            let fg = GetForegroundWindow();
            if item.windows.contains(&fg) && !IsIconic(fg).as_bool() {
                if item.windows.len() > 1 {
                    // Several windows: cycle to the one furthest back.
                    focus(*item.windows.last().unwrap());
                } else {
                    let _ = ShowWindow(fg, SW_MINIMIZE);
                }
            } else {
                focus(item.windows[0]);
            }
        }
    }

    fn launch(&mut self, index: usize) {
        let item = &self.items[index];
        let job = match item.pin {
            Some(p) => {
                let pin = &self.cfg.pins[p];
                Deferred::Launch(shell::expand_env(&pin.target), pin.args.clone())
            }
            None => match &item.exe {
                Some(exe) => Deferred::Launch(exe.clone(), String::new()),
                None => return,
            },
        };
        self.deferred.push(job);
    }

    fn set_hot(&mut self, hot: Option<usize>) {
        if hot != self.hot {
            self.hot = hot;
            if self.pressed.is_some() {
                self.draw();
            }
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_REFRESH => {
                hooks::refresh_handled();
                self.refresh(true);
            }
            WM_APP_MOUSE => {
                self.at_edge = wp.0 & MOUSE_AT_EDGE != 0;
                self.in_dock = wp.0 & MOUSE_IN_DOCK != 0;
                unsafe {
                    if self.at_edge && !self.hover_reveal {
                        SetTimer(self.icons_hwnd, TIMER_REVEAL, self.cfg.reveal_delay_ms, None);
                    } else if !self.at_edge {
                        let _ = KillTimer(self.icons_hwnd, TIMER_REVEAL);
                    }
                    if self.in_dock {
                        let _ = KillTimer(self.icons_hwnd, TIMER_HIDE);
                    } else if self.hover_reveal {
                        SetTimer(self.icons_hwnd, TIMER_HIDE, self.cfg.hide_delay_ms, None);
                    }
                }
            }
            WM_TIMER => {
                unsafe {
                    let _ = KillTimer(self.icons_hwnd, wp.0);
                }
                match wp.0 {
                    TIMER_REVEAL if self.at_edge => self.hover_reveal = true,
                    TIMER_HIDE if !self.in_dock => self.hover_reveal = false,
                    _ => {}
                }
                self.update_target();
            }
            WM_APP_CONFIG => {
                let cfg = config::load();
                if cfg != self.cfg {
                    self.apply_config(cfg);
                }
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => self.relayout(),
            WM_SETTINGCHANGE if wp.0 as u32 == SPI_SETWORKAREA.0 => self.relayout(),
            m if m == self.taskbar_created => {
                self.tray = find_tray();
                self.relayout();
            }
            WM_MOUSEMOVE => {
                if !self.tracking_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.icons_hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.tracking_leave = true;
                }
                self.set_hot(self.hit(lp));
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                self.set_hot(None);
            }
            WM_LBUTTONDOWN => {
                self.pressed = self.hit(lp);
                unsafe { SetCapture(self.icons_hwnd) };
                self.draw();
            }
            WM_LBUTTONUP => {
                let target = self.hit(lp);
                let pressed = self.pressed.take();
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.draw();
                if let Some(i) = target.filter(|&i| Some(i) == pressed) {
                    self.activate(i);
                }
            }
            WM_MBUTTONUP => {
                if let Some(i) = self.hit(lp) {
                    self.launch(i);
                }
            }
            WM_RBUTTONUP => {
                let index = self.hit(lp);
                self.deferred.push(Deferred::Menu(index));
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn apply_config(&mut self, cfg: Config) {
        self.matches = cfg.pins.iter().map(pin_match).collect();
        self.cfg = cfg;
        self.bitmaps.clear();
        self.refresh(false);
        self.relayout();
    }
}

fn focus(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            SwitchToThisWindow(hwnd, true);
        }
    }
}

// ---- context menu ---------------------------------------------------------------------------

const CMD_PIN: usize = 1;
const CMD_UNPIN: usize = 2;
const CMD_LOCATION: usize = 3;
const CMD_NEW: usize = 4;
const CMD_SETTINGS: usize = 10;
const CMD_QUIT: usize = 11;

fn show_menu(index: Option<usize>) {
    let prep = DOCK.with(|cell| {
        let mut guard = cell.borrow_mut();
        let dock = guard.as_mut()?;
        dock.menu_open = true;
        dock.update_target();
        let item = index.and_then(|i| dock.items.get(i).cloned());
        Some((dock.icons_hwnd, item))
    });
    let Some((hwnd, item)) = prep else { return };

    let cmd = unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let add = |id: usize, text: &str| {
            let _ = AppendMenuW(menu, MF_STRING, id, &HSTRING::from(text));
        };
        if let Some(item) = &item {
            if !item.windows.is_empty() {
                add(CMD_NEW, "New window");
            }
            if item.pin.is_some() {
                add(CMD_UNPIN, "Remove from dock");
            } else {
                add(CMD_PIN, "Keep in dock");
            }
            if item.exe.is_some() || item.pin.is_some() {
                add(CMD_LOCATION, "Open file location");
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        add(CMD_SETTINGS, "Dock settings (dock.toml)");
        add(CMD_QUIT, "Quit Lumen dock");

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_CENTERALIGN | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    };

    DOCK.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(dock) = guard.as_mut() else { return };
        dock.menu_open = false;
        dock.update_target();
        match (cmd, &item) {
            (CMD_NEW, Some(_)) => dock.launch(index.unwrap()),
            (CMD_PIN, Some(item)) => {
                if let Some(exe) = &item.exe {
                    dock.cfg.pins.push(Pin { name: item.name.clone(), target: exe.clone(), args: String::new(), exe: None });
                    config::save(&dock.cfg);
                    let cfg = dock.cfg.clone();
                    dock.apply_config(cfg);
                }
            }
            (CMD_UNPIN, Some(item)) => {
                if let Some(p) = item.pin {
                    dock.cfg.pins.remove(p);
                    config::save(&dock.cfg);
                    let cfg = dock.cfg.clone();
                    dock.apply_config(cfg);
                }
            }
            (CMD_LOCATION, Some(item)) => {
                let path = item.exe.clone().or_else(|| item.pin.map(|p| shell::expand_env(&dock.cfg.pins[p].target)));
                if let Some(path) = path.filter(|p| !p.to_lowercase().starts_with("shell:")) {
                    shell::open_in_explorer(&path);
                }
            }
            (CMD_SETTINGS, _) => dock.deferred.push(Deferred::Launch(config::path().display().to_string(), String::new())),
            (CMD_QUIT, _) => unsafe { PostQuitMessage(0) },
            _ => {}
        }
    });
}
