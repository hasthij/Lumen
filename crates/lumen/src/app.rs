//! The app shell: a hidden top-level window that owns the tray icon, receives system
//! broadcasts (sleep/resume, time change, display change, Explorer restart), and drives
//! module timers. Modules are created and dropped as they are toggled, so a module that
//! is off uses no memory at all.

use std::cell::RefCell;

use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clock::Clock;
use crate::config::Config;

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_CLOCK: usize = 1;
const TIMER_TRIM: usize = 2;

const CMD_CLOCK: usize = 100;
const CMD_RELOAD: usize = 101;
const CMD_OPEN_CONFIG: usize = 102;
const CMD_OPEN_LOG: usize = 103;
const CMD_QUIT: usize = 199;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub struct App {
    hwnd: HWND,
    cfg: Config,
    clock: Option<Clock>,
    taskbar_created: u32,
    icon: HICON,
}

pub fn run() -> Result<()> {
    let hwnd = unsafe { create_window()? };
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let cfg = Config::load();
    let app = App { hwnd, cfg, clock: None, taskbar_created, icon: crate::icon::create() };
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| {
        app.add_tray();
        app.apply_modules();
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

/// Runs `f` on the app unless it is already borrowed (a re-entrant message), in which case
/// the message is simply not handled by app logic.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl App {
    fn apply_modules(&mut self) {
        match (self.cfg.modules.clock, self.clock.is_some()) {
            (true, false) => match Clock::create(&self.cfg.clock) {
                Ok(c) => {
                    self.clock = Some(c);
                    self.clock_tick(true);
                    crate::info!("clock module on");
                }
                Err(e) => crate::error!("clock module failed to start: {e}"),
            },
            (false, true) => {
                self.clock = None;
                unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_CLOCK);
                }
                crate::info!("clock module off");
            }
            _ => {}
        }
        self.schedule_trim();
    }

    fn clock_tick(&mut self, force: bool) {
        let Some(clock) = self.clock.as_mut() else { return };
        let delay = clock.tick(force);
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
            let _ = AppendMenuW(menu, MF_STRING | check(self.cfg.modules.clock), CMD_CLOCK, w!("Clock"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, w!("Reload skin"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_CONFIG, w!("Open settings file"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_LOG, w!("Open log folder"));
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
            CMD_RELOAD => {
                if let Some(c) = self.clock.as_mut() {
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
                if event == WM_CONTEXTMENU || event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                    self.show_menu();
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_CLOCK => self.clock_tick(false),
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
    if let Some(Some(r)) = with_app(|app| app.handle(msg, wp, lp)) {
        return r;
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

unsafe fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    let _ = EmptyWorkingSet(GetCurrentProcess());
}
