//! Clear taskbar module (replaces TranslucentTB).
//!
//! Lumen loads its helper DLL (`lumen_tap.dll`) into Explorer through the XAML
//! diagnostics API, then tells it which colour the taskbar background should be. The look
//! follows what's on screen: desktop, a maximized window, Start/Search, or Task View.
//!
//! Fail-safe: if Explorer restarts within 20 seconds of Lumen loading the helper twice in a
//! row, the module turns itself off so a Windows update can never leave the shell crashing.

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{s, w, HSTRING};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{TaskbarConfig, TaskbarLook};

const WM_TAP_APPLY: u32 = WM_APP + 1;
const WM_TAP_RESTORE: u32 = WM_APP + 2;

/// Posted to the app window when something on screen changed; the app debounces it.
pub const WM_TASKBAR_EVENT: u32 = WM_APP + 30;
/// Timer id on the app window: re-run `update()` while the helper window isn't up yet.
pub const TAP_RETRY_TIMER: usize = 6;
static NOTIFY: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Situation {
    Desktop,
    Maximized,
    Start,
    TaskView,
}

pub struct Taskbar {
    cfg: TaskbarConfig,
    hooks: Vec<HWINEVENTHOOK>,
    situation: Option<Situation>,
    applied: Option<(String, u32)>,
    blur_on: bool,
    injected_at: Option<Instant>,
    explorer_pid: u32,
    quick_restarts: u32,
    /// How many updates found no helper window yet.
    waiting: u32,
    pub status: String,
    pub failed: bool,
}

impl Taskbar {
    pub fn create(cfg: &TaskbarConfig, notify: HWND) -> Self {
        NOTIFY.store(notify.0 as isize, Ordering::SeqCst);
        let mut tb = Self {
            cfg: cfg.clone(),
            hooks: Vec::new(),
            situation: None,
            applied: None,
            blur_on: false,
            injected_at: None,
            explorer_pid: 0,
            quick_restarts: 0,
            waiting: 0,
            status: "Starting".into(),
            failed: false,
        };
        unsafe {
            for (min, max) in [
                (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
                (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
                (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
            ] {
                let h = SetWinEventHook(min, max, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
                if !h.is_invalid() {
                    tb.hooks.push(h);
                }
            }
        }
        tb.attach();
        tb
    }

    pub fn set_config(&mut self, cfg: &TaskbarConfig) {
        self.cfg = cfg.clone();
        self.applied = None;
        self.update();
    }

    /// Loads the helper into Explorer unless it is already there.
    pub fn attach(&mut self) {
        let Some(pid) = explorer_pid() else {
            self.status = "Waiting for Explorer".into();
            return;
        };
        if tap_window().is_some_and(|h| window_pid(h) == pid) {
            self.explorer_pid = pid;
            self.status = "Connected".into();
            self.applied = None;
            self.update();
            return;
        }
        match inject(pid, &self.initial_data()) {
            Ok(()) => {
                crate::info!("taskbar helper loaded into Explorer (pid {pid})");
                self.injected_at = Some(Instant::now());
                self.explorer_pid = pid;
                self.status = "Connected".into();
                self.applied = None;
            }
            Err(e) => {
                crate::error!("could not load the taskbar helper into Explorer: {e}");
                self.status = format!("Couldn't connect to Explorer ({e})");
            }
        }
    }

    /// Explorer restarted (TaskbarCreated). Returns false when the fail-safe tripped.
    pub fn explorer_restarted(&mut self) -> bool {
        let quick = self.injected_at.is_some_and(|t| t.elapsed() < Duration::from_secs(20));
        self.quick_restarts = if quick { self.quick_restarts + 1 } else { 0 };
        if self.quick_restarts >= 2 {
            crate::error!("Explorer restarted twice right after loading the taskbar helper; turning the clear taskbar off");
            self.failed = true;
            self.status = "Turned off: Explorer restarted twice after loading it. Check for a Windows update".into();
            return false;
        }
        self.blur_on = false;
        self.applied = None;
        self.situation = None;
        true
    }

    fn initial_data(&self) -> String {
        let look = self.look(self.current_situation());
        match look.mode.as_str() {
            "normal" => String::new(),
            _ => format!("{:08X}", argb(look)),
        }
    }

    fn look(&self, s: Situation) -> &TaskbarLook {
        match s {
            Situation::Desktop => &self.cfg.desktop,
            Situation::Maximized => &self.cfg.maximized,
            Situation::Start => &self.cfg.start,
            Situation::TaskView => &self.cfg.task_view,
        }
    }

    fn current_situation(&self) -> Situation {
        situation()
    }

    /// Re-evaluates what is on screen and sends the matching look to the helper.
    pub fn update(&mut self) {
        if self.failed {
            return;
        }
        let s = self.current_situation();
        self.situation = Some(s);
        let look = self.look(s).clone();
        let key = (look.mode.clone(), argb(&look));
        if self.applied.as_ref() == Some(&key) {
            return;
        }
        let Some(tap) = tap_window() else {
            // The helper's window appears a moment after it loads. Ask the app to try again.
            self.status = "Waiting for the taskbar helper".into();
            self.waiting += 1;
            let notify = NOTIFY.load(Ordering::SeqCst);
            if notify != 0 && self.waiting <= 40 {
                unsafe {
                    SetTimer(HWND(notify as *mut _), TAP_RETRY_TIMER, 500, None);
                }
            } else if self.waiting > 40 {
                self.status = "The helper loaded in Explorer but isn't answering".into();
            }
            return;
        };
        self.waiting = 0;
        unsafe {
            let _ = match look.mode.as_str() {
                "normal" => PostMessageW(tap, WM_TAP_RESTORE, WPARAM(0), LPARAM(0)),
                "blur" => PostMessageW(tap, WM_TAP_APPLY, WPARAM(0), LPARAM(0)),
                _ => PostMessageW(tap, WM_TAP_APPLY, WPARAM(argb(&look) as usize), LPARAM(0)),
            };
        }
        // Blur needs the window's own composition; the other modes leave it as Windows set it.
        if look.mode == "blur" {
            set_accent(3, abgr(&look));
            self.blur_on = true;
        } else if self.blur_on {
            set_accent(0, 0);
            self.blur_on = false;
        }
        self.status = format!("Connected · {}", describe(s, &look));
        self.applied = Some(key);
    }
}

impl Drop for Taskbar {
    fn drop(&mut self) {
        unsafe {
            for h in self.hooks.drain(..) {
                let _ = UnhookWinEvent(h);
            }
            if let Some(tap) = tap_window() {
                let _ = PostMessageW(tap, WM_TAP_RESTORE, WPARAM(0), LPARAM(0));
            }
        }
        if self.blur_on {
            set_accent(0, 0);
        }
    }
}

fn describe(s: Situation, look: &TaskbarLook) -> String {
    let where_ = match s {
        Situation::Desktop => "desktop",
        Situation::Maximized => "maximized window",
        Situation::Start => "Start",
        Situation::TaskView => "Task View",
    };
    format!("{} ({where_})", look.mode)
}

fn argb(look: &TaskbarLook) -> u32 {
    match look.mode.as_str() {
        "clear" | "blur" => 0,
        _ => {
            let c = crate::theme::parse_hex(&look.color).unwrap_or([0.0; 3]);
            let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
            (b(look.opacity) << 24) | (b(c[0]) << 16) | (b(c[1]) << 8) | b(c[2])
        }
    }
}

/// Accent gradient colour is ABGR.
fn abgr(look: &TaskbarLook) -> u32 {
    let c = crate::theme::parse_hex(&look.color).unwrap_or([0.0; 3]);
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (b(look.opacity) << 24) | (b(c[2]) << 16) | (b(c[1]) << 8) | b(c[0])
}

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, _: i32, _: u32, _: u32) {
    // Location changes fire for carets and cursors too; only whole windows matter.
    if event == EVENT_OBJECT_LOCATIONCHANGE && (id_object != OBJID_WINDOW.0 || !IsZoomed(hwnd).as_bool() && !was_zoomed(hwnd)) {
        return;
    }
    if (event == EVENT_OBJECT_DESTROY || event == EVENT_OBJECT_HIDE) && id_object != OBJID_WINDOW.0 {
        return;
    }
    let notify = NOTIFY.load(Ordering::SeqCst);
    if notify != 0 {
        let _ = PostMessageW(HWND(notify as *mut _), WM_TASKBAR_EVENT, WPARAM(0), LPARAM(0));
    }
}

/// Un-maximizing fires a location change on a window that is no longer zoomed; any normal
/// top-level window move is cheap enough to re-check.
fn was_zoomed(hwnd: HWND) -> bool {
    unsafe { GetParent(hwnd).is_err() && IsWindowVisible(hwnd).as_bool() }
}

fn situation() -> Situation {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_invalid() {
            let class = class_name(fg);
            let title = window_text(fg);
            if class == "XamlExplorerHostIslandWindow" && title.contains("Task View") || class == "MultitaskingViewFrame" {
                return Situation::TaskView;
            }
            if class == "Windows.UI.Core.CoreWindow" {
                let exe = crate::launcher::shell::process_image(window_pid(fg)).unwrap_or_default().to_lowercase();
                if exe.ends_with("startmenuexperiencehost.exe") || exe.ends_with("searchhost.exe") || exe.ends_with("searchapp.exe") {
                    return Situation::Start;
                }
            }
        }
        if any_maximized() {
            Situation::Maximized
        } else {
            Situation::Desktop
        }
    }
}

fn any_maximized() -> bool {
    unsafe extern "system" fn check(hwnd: HWND, lp: LPARAM) -> BOOL {
        let found = &mut *(lp.0 as *mut bool);
        if IsWindowVisible(hwnd).as_bool() && IsZoomed(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
            if cloaked == 0 {
                *found = true;
                return false.into();
            }
        }
        true.into()
    }
    let mut found = false;
    unsafe {
        let _ = EnumWindows(Some(check), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn window_text(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn window_pid(h: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

fn explorer_pid() -> Option<u32> {
    let tray = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.ok()?;
    Some(window_pid(tray)).filter(|&p| p != 0)
}

/// Puts the taskbar back exactly as Windows draws it: tells the helper (if it is loaded)
/// to drop Lumen's colours and clears any composition Lumen set. Safe to call any time.
pub fn restore_windows_look() {
    unsafe {
        if let Some(tap) = tap_window() {
            let _ = PostMessageW(tap, WM_TAP_RESTORE, WPARAM(0), LPARAM(0));
        }
    }
    set_accent(0, 0);
}

/// Is Lumen's helper still loaded in Explorer? (It stays until Explorer restarts.)
pub fn helper_loaded() -> bool {
    tap_window().is_some()
}

/// Deletes the helper copies. Only succeeds for files Explorer no longer has loaded.
pub fn remove_helper_files() {
    let _ = std::fs::remove_dir_all(crate::log::dir().join("taskbar"));
}

/// Restarts Explorer, which also unloads Lumen's helper from it completely.
pub fn restart_explorer() {
    crate::info!("restarting Explorer to unload the taskbar helper");
    crate::launcher::shell::execute("open", "cmd.exe", "/c taskkill /f /im explorer.exe & start explorer.exe", None);
}

fn tap_window() -> Option<HWND> {
    unsafe { FindWindowExW(HWND_MESSAGE, None, w!("LumenTapWnd"), None) }.ok().filter(|h| !h.is_invalid())
}

/// Is TranslucentTB running? (Both styling the taskbar at once fight each other.)
pub fn translucenttb_running() -> bool {
    use windows::Win32::System::ProcessStatus::EnumProcesses;
    let mut pids = vec![0u32; 2048];
    let mut bytes = 0u32;
    unsafe {
        if EnumProcesses(pids.as_mut_ptr(), (pids.len() * 4) as u32, &mut bytes).is_err() {
            return false;
        }
    }
    pids.truncate(bytes as usize / 4);
    pids.into_iter().any(|pid| {
        crate::launcher::shell::process_image(pid).is_some_and(|p| p.to_lowercase().ends_with("translucenttb.exe"))
    })
}

/// The helper DLL is copied to a versioned name first: Explorer keeps it loaded until it
/// restarts, so updating Lumen must never need to overwrite a file Explorer holds.
fn helper_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let src = exe.with_file_name("lumen_tap.dll");
    let meta = std::fs::metadata(&src).map_err(|_| format!("{} is missing", src.display()))?;
    let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let dir = crate::log::dir().join("taskbar");
    let _ = std::fs::create_dir_all(&dir);
    let dst = dir.join(format!("lumen_tap_{}_{stamp}.dll", meta.len()));
    if !dst.exists() {
        std::fs::copy(&src, &dst).map_err(|e| format!("copy helper: {e}"))?;
        // Tidy older copies Explorer no longer has open (deleting an in-use one just fails).
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path() != dst {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    Ok(dst)
}

/// Gets the helper into Explorer the way TranslucentTB does: a temporary window hook on
/// the taskbar's thread makes Windows load the DLL there, and the DLL connects XAML
/// diagnostics from inside Explorer. The hook is removed once the helper reports back.
fn inject(_pid: u32, _data: &str) -> Result<(), String> {
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
    let dll = helper_path()?;
    unsafe {
        let tray = FindWindowW(w!("Shell_TrayWnd"), None).map_err(|_| "taskbar not found".to_string())?;
        let tid = GetWindowThreadProcessId(tray, None);
        let ready = CreateEventW(None, true, false, w!("Local\\LumenTapReady")).map_err(|e| e.to_string())?;
        let _ = ResetEvent(ready);
        let module = LoadLibraryW(&HSTRING::from(dll.as_os_str())).map_err(|e| format!("load helper: {e}"))?;
        let result = (|| {
            let proc = GetProcAddress(module, s!("CallWndProc")).ok_or("helper has no hook procedure")?;
            let hook = SetWindowsHookExW(WH_CALLWNDPROC, Some(std::mem::transmute(proc)), module, tid).map_err(|e| format!("hook: {e}"))?;
            // Any message to the taskbar thread makes Windows load the DLL there.
            let _ = SendMessageTimeoutW(tray, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 2000, None);
            let waited = WaitForSingleObject(ready, 15_000);
            let _ = UnhookWindowsHookEx(hook);
            if waited.0 != 0 {
                return Err("Explorer didn't answer in time".to_string());
            }
            Ok(())
        })();
        let _ = FreeLibrary(module);
        let _ = windows::Win32::Foundation::CloseHandle(ready);
        result
    }
}

/// SetWindowCompositionAttribute (undocumented, stable since Windows 10) on every taskbar.
fn set_accent(state: u32, gradient: u32) {
    #[repr(C)]
    struct Accent {
        state: u32,
        flags: u32,
        gradient: u32,
        animation: u32,
    }
    #[repr(C)]
    struct Data {
        attrib: u32,
        data: *mut Accent,
        size: usize,
    }
    type SwcaFn = unsafe extern "system" fn(HWND, *mut Data) -> BOOL;
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else { return };
        let swca: SwcaFn = std::mem::transmute(f);
        let mut accent = Accent { state, flags: 2, gradient, animation: 0 };
        for class in [w!("Shell_TrayWnd"), w!("Shell_SecondaryTrayWnd")] {
            let mut after = HWND::default();
            while let Ok(h) = FindWindowExW(None, after, class, None) {
                if h.is_invalid() {
                    break;
                }
                let mut data = Data { attrib: 19, data: &mut accent, size: std::mem::size_of::<Accent>() };
                let _ = swca(h, &mut data);
                after = h;
            }
        }
    }
}
