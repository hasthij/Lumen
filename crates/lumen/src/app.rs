//! The app shell: a hidden top-level window that owns the tray icon, receives system
//! broadcasts (sleep/resume, time change, display change, Explorer restart), and drives
//! module timers. Modules are created and dropped as they are toggled, so a module that
//! is off uses no memory at all.

use std::cell::RefCell;

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clock::Clock;
use crate::config::ClockConfig;
use crate::config::Config;
use crate::launcher::Launcher;
use crate::settings::{Change, Settings, Status};
use crate::taskbar::{Taskbar, WM_TASKBAR_EVENT};

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by a second `lumen.exe --launcher` to open the launcher.
pub const WM_OPEN_LAUNCHER: u32 = WM_APP + 2;
/// Posted by a second plain `lumen.exe` to open settings.
pub const WM_OPEN_SETTINGS: u32 = WM_APP + 3;
/// Posted by the clock window after it was dragged in move mode.
pub const WM_CLOCK_MOVED: u32 = WM_APP + 4;
/// Posted by `lumen --restore`: turn the taskbar module off and restore the taskbar.
pub const WM_RESTORE_TASKBAR: u32 = WM_APP + 5;
/// Posted by the foreground watcher whenever the active window changes.
const WM_FOREGROUND: u32 = WM_APP + 6;
static APP_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn on_foreground(_: windows::Win32::UI::Accessibility::HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    let app = APP_HWND.load(std::sync::atomic::Ordering::Relaxed);
    if app != 0 {
        let _ = PostMessageW(HWND(app as *mut _), WM_FOREGROUND, WPARAM(0), LPARAM(0));
    }
}

/// Is the desktop in front? `None` for windows that shouldn't change the answer (the
/// taskbar, tray overflow, Lumen's own windows), so clicking them keeps the clock up.
fn desktop_in_front() -> Option<bool> {
    unsafe {
        let fg = GetForegroundWindow();
        let mut buf = [0u16; 64];
        let n = GetClassNameW(fg, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        match class.as_str() {
            "Progman" | "WorkerW" => Some(true),
            // Right-clicking the desktop opens a menu ("#32768") that briefly becomes the
            // foreground window; it must not count as "an app is in front", or the clock
            // would drop behind the desktop while the menu is open.
            c if c.starts_with('#') => None,
            "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "NotifyIconOverflowWindow" | "TopLevelWindowForOverflowXamlIsland" | "XamlExplorerHostIslandWindow" | "ForegroundStaging" | "LumenLauncher" | "LumenSettings" | "LumenApp" | "" => None,
            _ => {
                // Tool windows, tooltips and similar helpers aren't apps either.
                let ex = GetWindowLongPtrW(fg, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOOLWINDOW.0 != 0 { None } else { Some(false) }
            }
        }
    }
}
const TIMER_CLOCK: usize = 1;
const TIMER_TRIM: usize = 2;
const TIMER_HOTKEY_RETRY: usize = 3;
const TIMER_TASKBAR: usize = 4;
const TIMER_TASKBAR_ATTACH: usize = 5;
const HOTKEY_LAUNCHER: i32 = 1;

const CMD_CLOCK: usize = 100;
const CMD_RELOAD: usize = 101;
const CMD_LAUNCHER: usize = 104;
const CMD_OPEN_LAUNCHER: usize = 105;
const CMD_SETTINGS: usize = 106;
const CMD_TASKBAR: usize = 107;
const CMD_RESTORE_TASKBAR: usize = 108;
const CMD_OPEN_CONFIG: usize = 102;
const CMD_OPEN_LOG: usize = 103;
const CMD_QUIT: usize = 199;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub struct App {
    hwnd: HWND,
    cfg: Config,
    /// One window per skin that is switched on.
    skins: Vec<Clock>,
    launcher: Option<Launcher>,
    settings: Option<Settings>,
    taskbar: Option<Taskbar>,
    /// Why the taskbar module last stopped on its own, shown in settings.
    taskbar_note: String,
    hotkey_ok: bool,
    taskbar_created: u32,
    icon: HICON,
}

pub fn run(open_settings: bool) -> Result<()> {
    let hwnd = unsafe { create_window()? };
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    APP_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
    // One out-of-process hook for foreground changes; Windows calls us only when the
    // active window changes, so this costs nothing in between.
    let _foreground_hook = unsafe {
        use windows::Win32::UI::Accessibility::SetWinEventHook;
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
    };
    let cfg = Config::load();
    let app = App { hwnd, cfg, skins: Vec::new(), launcher: None, settings: None, taskbar: None, taskbar_note: String::new(), hotkey_ok: false, taskbar_created, icon: crate::icon::create() };
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| {
        app.add_tray();
        app.apply_modules();
        if open_settings {
            app.open_settings();
        }
    });

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().take() {
            app.remove_tray();
        }
    });
    Ok(())
}

/// Messages for the launcher window, routed to the launcher module if it is on.
pub fn launcher_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    with_app(|app| app.launcher.as_mut().and_then(|l| l.handle(msg, wp, lp))).flatten()
}

/// Messages for the settings window. Changes it makes are applied right after.
pub fn settings_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    with_app(|app| {
        if msg == WM_TIMER && wp.0 == 2 {
            app.refresh_settings();
        }
        let r = app.settings.as_mut().and_then(|s| s.handle(msg, wp, lp));
        app.after_settings();
        r
    })
    .flatten()
}

/// Runs `f` on the app unless it is already borrowed (a re-entrant message), in which case
/// the message is simply not handled by app logic.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl App {
    fn apply_modules(&mut self) {
        self.sync_skins();
        match (self.cfg.modules.taskbar, self.taskbar.is_some()) {
            (true, false) => {
                self.taskbar_note.clear();
                self.taskbar = Some(Taskbar::create(&self.cfg.taskbar, self.hwnd));
                self.taskbar_update();
                crate::info!("taskbar module on");
            }
            (false, true) => {
                self.taskbar = None;
                crate::info!("taskbar module off");
            }
            _ => {}
        }
        match (self.cfg.modules.launcher, self.launcher.is_some()) {
            (true, false) => match Launcher::create(&self.cfg.launcher, &self.cfg.appearance) {
                Ok(l) => {
                    self.launcher = Some(l);
                    self.register_hotkey();
                    crate::info!("launcher module on");
                }
                Err(e) => crate::error!("launcher module failed to start: {e}"),
            },
            (false, true) => {
                unsafe {
                    let _ = UnregisterHotKey(self.hwnd, HOTKEY_LAUNCHER);
                    let _ = KillTimer(self.hwnd, TIMER_HOTKEY_RETRY);
                }
                self.hotkey_ok = false;
                self.launcher = None;
                crate::info!("launcher module off");
            }
            _ => {}
        }
        self.schedule_trim();
    }

    /// Makes the skin windows match the config: windows for skins that were switched on,
    /// settings pushed to ones already running, windows closed for skins switched off.
    fn sync_skins(&mut self) {
        let want: Vec<ClockConfig> = if self.cfg.modules.clock { self.cfg.skins.iter().filter(|s| s.enabled).cloned().collect() } else { Vec::new() };
        self.skins.retain(|c| want.iter().any(|w| w.skin.eq_ignore_ascii_case(c.rel())));
        let root = self.cfg.clock.skins_path.clone();
        for w in &want {
            match self.skins.iter_mut().find(|c| c.rel().eq_ignore_ascii_case(&w.skin)) {
                Some(c) => c.apply_config(w),
                None => match Clock::create(w, &root) {
                    Ok(c) => {
                        crate::info!("skin on: {}", w.skin);
                        self.skins.push(c);
                    }
                    Err(e) => crate::error!("skin {} failed to start: {e}", w.skin),
                },
            }
        }
        if self.skins.is_empty() {
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_CLOCK);
            }
        } else {
            self.clock_tick(true);
        }
    }

    pub fn status(&self) -> Status {
        Status {
            hotkey_ok: self.hotkey_ok,
            skins_running: self.skins.iter().filter(|c| c.has_skin()).count(),
            skin_notes: self
                .skins
                .iter()
                .map(|c| {
                    let un = c.unsupported();
                    let note = if !c.has_skin() { "Couldn't be read. Check the log".to_string() } else if un.is_empty() { String::new() } else { format!("Not drawn yet: {}", un.join(", ")) };
                    (c.rel().to_string(), note)
                })
                .collect(),
            memory_mb: memory_mb(),
            autostart: crate::autostart::enabled(),
            apps: self.launcher.as_ref().map_or(0, |l| l.app_count()),
            taskbar: match &self.taskbar {
                Some(t) => t.status.clone(),
                None if crate::taskbar::helper_loaded() => {
                    "Off. The helper stays loaded in Explorer until Explorer restarts".into()
                }
                None if !self.taskbar_note.is_empty() => self.taskbar_note.clone(),
                None => "Off".into(),
            },
            translucenttb: crate::taskbar::translucenttb_running(),
            moving_skin: self.skins.iter().find(|c| c.moving()).map(|c| c.rel().to_string()).unwrap_or_default(),
            skins_dir: if self.cfg.clock.skins_path.is_empty() {
                crate::rainmeter::Install::locate().skins.map(|p| p.display().to_string()).unwrap_or_default()
            } else {
                self.cfg.clock.skins_path.clone()
            },
        }
    }

    fn taskbar_update(&mut self) {
        if let Some(t) = self.taskbar.as_mut() {
            t.update();
        }
    }

    /// Applies a new configuration to every running module. `save` is false while a
    /// slider is being dragged (live preview) and true when it is released.
    fn apply_config(&mut self, cfg: Config, save: bool) {
        let hotkey_changed = cfg.launcher.hotkey != self.cfg.launcher.hotkey;
        self.cfg = cfg;
        if save {
            self.cfg.save();
        }
        self.apply_modules();
        if let Some(l) = self.launcher.as_mut() {
            l.set_config(&self.cfg.launcher, &self.cfg.appearance);
        }
        if hotkey_changed && self.launcher.is_some() {
            self.hotkey_ok = false;
            self.register_hotkey();
        }
        if let Some(t) = self.taskbar.as_mut() {
            t.set_config(&self.cfg.taskbar);
        }
    }

    fn open_settings(&mut self) {
        if let Some(s) = &self.settings {
            s.focus();
            return;
        }
        match Settings::open(&self.cfg, self.status()) {
            Ok(s) => self.settings = Some(s),
            Err(e) => crate::error!("could not open settings: {e}"),
        }
    }

    /// Undo for the taskbar: module off (saved), helper told to restore, composition reset.
    fn restore_taskbar(&mut self) {
        self.taskbar = None;
        self.cfg.modules.taskbar = false;
        self.cfg.save();
        crate::taskbar::restore_windows_look();
        self.taskbar_note = "Restored to Windows' normal taskbar".into();
        crate::info!("taskbar restored to normal");
        self.sync_settings();
    }

    /// The configuration changed outside the settings window (tray, clock dragged).
    fn sync_settings(&mut self) {
        let cfg = self.cfg.clone();
        let status = self.status();
        if let Some(s) = self.settings.as_mut() {
            s.sync(&cfg);
            s.set_status(status);
        }
    }

    fn refresh_settings(&mut self) {
        let status = self.status();
        if let Some(s) = self.settings.as_mut() {
            s.set_status(status);
        }
    }

    /// Applies what the settings window changed; drops the window once it is closed.
    fn after_settings(&mut self) {
        let Some(s) = self.settings.as_mut() else { return };
        let changes = s.take_changes();
        let closed = s.closed;
        let any = !changes.is_empty();
        for change in changes {
            match change {
                Change::Config(cfg) => self.apply_config(*cfg, true),
                Change::Preview(cfg) => self.apply_config(*cfg, false),
                Change::MoveSkin(which) => {
                    for c in self.skins.iter_mut() {
                        let on = which.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(c.rel()));
                        c.set_moving(on);
                    }
                    self.clock_tick(true);
                }
                Change::RestoreTaskbar => self.restore_taskbar(),
                Change::RestartExplorer => {
                    self.restore_taskbar();
                    crate::taskbar::restart_explorer();
                }
                Change::QuitTranslucentTB => {
                    crate::launcher::shell::execute("open", "taskkill.exe", "/IM TranslucentTB.exe", None);
                }
                Change::Autostart(on) => {
                    crate::autostart::set(on);
                }
                Change::ReloadSkin => {
                    for c in self.skins.iter_mut() {
                        c.reload();
                    }
                    self.clock_tick(true);
                }
                Change::RecordingHotkey(true) => unsafe {
                    let _ = UnregisterHotKey(self.hwnd, HOTKEY_LAUNCHER);
                    let _ = KillTimer(self.hwnd, TIMER_HOTKEY_RETRY);
                },
                Change::RecordingHotkey(false) => {
                    if self.launcher.is_some() {
                        self.register_hotkey();
                    }
                }
            }
        }
        if closed {
            self.settings = None;
            self.schedule_trim();
        } else if any {
            self.refresh_settings();
        }
    }

    /// Registers the launcher hotkey. If another app holds it (PowerToys Run uses
    /// Alt+Space too), retry every 15 s so Lumen takes over as soon as it is free.
    fn register_hotkey(&mut self) {
        let Some((mods, vk)) = parse_hotkey(&self.cfg.launcher.hotkey) else {
            crate::error!("invalid launcher hotkey '{}'", self.cfg.launcher.hotkey);
            return;
        };
        unsafe {
            let _ = UnregisterHotKey(self.hwnd, HOTKEY_LAUNCHER);
            match RegisterHotKey(self.hwnd, HOTKEY_LAUNCHER, mods | MOD_NOREPEAT, vk) {
                Ok(()) => {
                    if !self.hotkey_ok {
                        crate::info!("launcher hotkey {} registered", self.cfg.launcher.hotkey);
                    }
                    self.hotkey_ok = true;
                    let _ = KillTimer(self.hwnd, TIMER_HOTKEY_RETRY);
                }
                Err(e) => {
                    if self.hotkey_ok || GetLastErrorOnce::first() {
                        crate::warn!("{} is taken by another app ({e}); will keep retrying", self.cfg.launcher.hotkey);
                    }
                    self.hotkey_ok = false;
                    SetTimer(self.hwnd, TIMER_HOTKEY_RETRY, 15_000, None);
                }
            }
        }
    }

    fn clock_tick(&mut self, force: bool) {
        if self.skins.is_empty() {
            return;
        }
        // One timer for all skins: wake when the soonest one needs a redraw.
        let delay = self.skins.iter_mut().map(|c| c.tick(force)).min().unwrap_or(30_000);
        unsafe {
            // Coalescable: lets Windows batch our wake-up with others to save power.
            SetCoalescableTimer(self.hwnd, TIMER_CLOCK, delay, None, 30);
        }
    }

    /// Shortly after startup or a module change, hand unused pages back to Windows.
    fn schedule_trim(&self) {
        unsafe {
            SetTimer(self.hwnd, TIMER_TRIM, 3_000, None);
        }
    }

    fn add_tray(&self) {
        let mut nid = self.nid();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.icon;
        copy_wide(&mut nid.szTip, "Lumen");
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn remove_tray(&self) {
        let nid = self.nid();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn show_menu(&mut self) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Settings"));
            if self.launcher.is_some() {
                let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_LAUNCHER, w!("Open launcher"));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING | check(self.cfg.modules.clock), CMD_CLOCK, w!("Skins"));
            let _ = AppendMenuW(menu, MF_STRING | check(self.cfg.modules.launcher), CMD_LAUNCHER, w!("Launcher"));
            let _ = AppendMenuW(menu, MF_STRING | check(self.cfg.modules.taskbar), CMD_TASKBAR, w!("Clear taskbar"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, w!("Reload skin"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_CONFIG, w!("Open settings file"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_LOG, w!("Open log folder"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_RESTORE_TASKBAR, w!("Restore normal taskbar"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit Lumen"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, self.hwnd, None);
            let _ = DestroyMenu(menu);
            self.command(cmd.0 as usize);
        }
    }

    fn command(&mut self, cmd: usize) {
        match cmd {
            CMD_CLOCK => {
                self.cfg.modules.clock = !self.cfg.modules.clock;
                self.cfg.save();
                self.apply_modules();
            }
            CMD_SETTINGS => self.open_settings(),
            CMD_RESTORE_TASKBAR => self.restore_taskbar(),
            CMD_TASKBAR => {
                self.cfg.modules.taskbar = !self.cfg.modules.taskbar;
                self.cfg.save();
                self.apply_modules();
                self.sync_settings();
            }
            CMD_LAUNCHER => {
                self.cfg.modules.launcher = !self.cfg.modules.launcher;
                self.cfg.save();
                self.apply_modules();
            }
            CMD_OPEN_LAUNCHER => {
                if let Some(l) = self.launcher.as_mut() {
                    l.show();
                }
            }
            CMD_RELOAD => {
                for c in self.skins.iter_mut() {
                    c.reload();
                }
                self.clock_tick(true);
            }
            CMD_OPEN_CONFIG => open(&crate::config::path().to_string_lossy()),
            CMD_OPEN_LOG => open(&crate::log::dir().to_string_lossy()),
            CMD_QUIT => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
            _ => {}
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TRAY => {
                // NOTIFYICON_VERSION_4: the event is in the low word of lParam.
                let event = (lp.0 & 0xFFFF) as u32;
                if event == WM_CONTEXTMENU || event == WM_RBUTTONUP {
                    self.show_menu();
                } else if event == WM_LBUTTONUP || event == NIN_SELECT || event == NIN_SELECT + 1 {
                    self.open_settings();
                }
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                crate::info!("shortcut pressed");
                if wp.0 as i32 == HOTKEY_LAUNCHER {
                    if let Some(l) = self.launcher.as_mut() {
                        l.toggle(true);
                    }
                }
                Some(LRESULT(0))
            }
            WM_TASKBAR_EVENT => {
                // Bursts of window events collapse into one check 60 ms later.
                unsafe {
                    SetTimer(self.hwnd, TIMER_TASKBAR, 60, None);
                }
                Some(LRESULT(0))
            }
            WM_CLOCK_MOVED => {
                // The window that was dragged says who it is (wParam), so every skin keeps its own spot.
                let h = wp.0 as isize;
                let moved = self.skins.iter_mut().find(|c| c.hwnd().0 as isize == h).map(|c| (c.rel().to_string(), c.read_position()));
                if let Some((rel, (x, y))) = moved {
                    if let Some(s) = self.cfg.skin_mut(&rel) {
                        s.x = Some(x);
                        s.y = Some(y);
                    }
                    self.cfg.save();
                    crate::info!("{rel} moved to {x},{y}");
                }
                self.sync_settings();
                Some(LRESULT(0))
            }
            WM_FOREGROUND => {
                if let Some(front) = desktop_in_front() {
                    for c in self.skins.iter_mut() {
                        c.set_desktop_in_front(front);
                    }
                }
                Some(LRESULT(0))
            }
            WM_RESTORE_TASKBAR => {
                self.restore_taskbar();
                Some(LRESULT(0))
            }
            WM_OPEN_SETTINGS => {
                self.open_settings();
                Some(LRESULT(0))
            }
            WM_OPEN_LAUNCHER => {
                if let Some(l) = self.launcher.as_mut() {
                    l.show();
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_CLOCK => self.clock_tick(false),
                    TIMER_TASKBAR => unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_TASKBAR);
                        self.taskbar_update();
                    },
                    crate::taskbar::TAP_RETRY_TIMER => {
                        unsafe {
                            let _ = KillTimer(self.hwnd, crate::taskbar::TAP_RETRY_TIMER);
                        }
                        self.taskbar_update();
                        self.refresh_settings();
                    }
                    TIMER_TASKBAR_ATTACH => {
                        unsafe {
                            let _ = KillTimer(self.hwnd, TIMER_TASKBAR_ATTACH);
                        }
                        if let Some(t) = self.taskbar.as_mut() {
                            t.attach();
                            t.update();
                        }
                        self.refresh_settings();
                    }
                    TIMER_HOTKEY_RETRY => {
                        if self.launcher.is_some() {
                            self.register_hotkey();
                        }
                    }
                    TIMER_TRIM => unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_TRIM);
                        trim_working_set();
                    },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_POWERBROADCAST => {
                if wp.0 as u32 == PBT_APMRESUMEAUTOMATIC || wp.0 as u32 == PBT_APMRESUMESUSPEND {
                    crate::info!("resumed from sleep");
                    self.clock_tick(true);
                }
                Some(LRESULT(1))
            }
            WM_TIMECHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
                self.clock_tick(true);
                None
            }
            WM_SETTINGCHANGE => {
                // Work area or locale changes can move or re-text the clock.
                self.clock_tick(true);
                None
            }
            m if m == self.taskbar_created && m != 0 => {
                // Explorer restarted: the tray icon is gone, put it back.
                crate::info!("Explorer restarted, re-adding tray icon");
                self.add_tray();
                if let Some(t) = self.taskbar.as_mut() {
                    if t.explorer_restarted() {
                        // Give the new taskbar a moment to build its XAML tree.
                        unsafe {
                            SetTimer(self.hwnd, TIMER_TASKBAR_ATTACH, 3000, None);
                        }
                    } else {
                        self.taskbar_note = t.status.clone();
                        self.taskbar = None;
                        self.cfg.modules.taskbar = false;
                        self.cfg.save();
                        self.sync_settings();
                    }
                }
                self.clock_tick(true);
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("LumenApp"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    // A hidden top-level window (not message-only) so it receives broadcasts such as
    // TaskbarCreated, WM_TIMECHANGE and WM_DISPLAYCHANGE.
    CreateWindowExW(WS_EX_TOOLWINDOW, w!("LumenApp"), w!("Lumen"), WS_POPUP, 0, 0, 0, 0, None, None, instance, None)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match with_app(|app| app.handle(msg, wp, lp)) {
        Some(Some(r)) => return r,
        // The app was busy with another message (re-entrancy). A dropped shortcut press would
        // look like the keybind "stopped working", so queue it again.
        None if msg == WM_HOTKEY => {
            crate::warn!("shortcut arrived while Lumen was busy; retrying");
            let _ = PostMessageW(hwnd, msg, wp, lp);
            return LRESULT(0);
        }
        _ => {}
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn open(target: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().chain(std::iter::once(0))) {
        *d = c;
    }
}

fn memory_mb() -> f32 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    c.WorkingSetSize as f32 / (1024.0 * 1024.0)
}

unsafe fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    let _ = EmptyWorkingSet(GetCurrentProcess());
}

/// Logs a taken hotkey once per run instead of every retry.
struct GetLastErrorOnce;

impl GetLastErrorOnce {
    fn first() -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SEEN: AtomicBool = AtomicBool::new(false);
        !SEEN.swap(true, Ordering::Relaxed)
    }
}

/// "Alt+Space", "Win+Alt+Space", "Ctrl+Shift+K", "Alt+F1" → (modifiers, virtual key).
pub fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut key = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "alt" => mods |= MOD_ALT,
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" => mods |= MOD_WIN,
            "space" => key = Some(VK_SPACE.0 as u32),
            "enter" => key = Some(VK_RETURN.0 as u32),
            "tab" => key = Some(VK_TAB.0 as u32),
            p if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphanumeric()) => key = Some(p.to_ascii_uppercase().as_bytes()[0] as u32),
            p if p.starts_with('f') && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                key = Some(VK_F1.0 as u32 + p[1..].parse::<u32>().ok()? - 1)
            }
            _ => return None,
        }
    }
    Some((mods, key?))
}
