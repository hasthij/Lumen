//! Snapshot of top-level windows: which ones belong on the dock, and whether a maximized or
//! fullscreen window covers the dock's monitor.

use std::collections::HashMap;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONULL};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::shell;

pub struct AppWindow {
    pub hwnd: HWND,
    /// Full image path of the owning process (the hosted app's, for ApplicationFrameHost windows).
    pub exe: String,
    pub aumid: Option<String>,
}

pub struct Snapshot {
    /// Dock-worthy windows in z-order, topmost first.
    pub windows: Vec<AppWindow>,
    pub foreground: HWND,
    /// A maximized window is on the dock's monitor.
    pub covered: bool,
    /// The foreground window fills the whole monitor (games, video, F11).
    pub fullscreen: bool,
}

/// Caches pid -> image path; pruned to the pids seen in the latest snapshot.
#[derive(Default)]
pub struct ExeCache(HashMap<u32, String>);

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    (*(lparam.0 as *mut Vec<HWND>)).push(hwnd);
    true.into()
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0
    }
}

fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    r
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

const SHELL_CLASSES: &[&str] = &["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

/// UWP apps live in a CoreWindow hosted by ApplicationFrameHost; find the real process.
fn hosted_pid(frame: HWND) -> Option<u32> {
    let frame_pid = pid_of(frame);
    let mut child = unsafe { FindWindowExW(frame, None, None, None) }.ok()?;
    loop {
        if class_name(child) == "Windows.UI.Core.CoreWindow" {
            let pid = pid_of(child);
            if pid != frame_pid {
                return Some(pid);
            }
        }
        child = unsafe { FindWindowExW(frame, child, None, None) }.ok()?;
    }
}

impl ExeCache {
    pub fn snapshot(&mut self, own: &[HWND], monitor: HMONITOR, mon_rect: RECT) -> Snapshot {
        let mut all: Vec<HWND> = Vec::with_capacity(256);
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
        }
        let foreground = unsafe { GetForegroundWindow() };
        let mut windows = Vec::new();
        let mut covered = false;
        let mut fullscreen = false;
        let mut seen = HashMap::new();

        for hwnd in all {
            if own.contains(&hwnd) || !unsafe { IsWindowVisible(hwnd) }.as_bool() || is_cloaked(hwnd) {
                continue;
            }
            let class = class_name(hwnd);
            if SHELL_CLASSES.contains(&class.as_str()) {
                continue;
            }
            let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            let iconic = unsafe { IsIconic(hwnd) }.as_bool();
            let on_monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) } == monitor;

            if !iconic && on_monitor {
                if unsafe { IsZoomed(hwnd) }.as_bool() {
                    covered = true;
                }
                if hwnd == foreground {
                    let r = window_rect(hwnd);
                    if r.left <= mon_rect.left && r.top <= mon_rect.top && r.right >= mon_rect.right && r.bottom >= mon_rect.bottom {
                        fullscreen = true;
                        covered = true;
                    }
                }
            }

            // Taskbar-style filtering: no tool windows, no owned windows (unless they opt in).
            let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.unwrap_or_default();
            let app_window = ex & WS_EX_APPWINDOW.0 != 0;
            if ex & WS_EX_TOOLWINDOW.0 != 0 && !app_window {
                continue;
            }
            if !owner.is_invalid() && !app_window {
                continue;
            }
            if ex & WS_EX_NOACTIVATE.0 != 0 && !app_window {
                continue;
            }
            if unsafe { GetWindowTextLengthW(hwnd) } == 0 {
                continue;
            }

            let pid = if class == "ApplicationFrameWindow" {
                match hosted_pid(hwnd) {
                    Some(pid) => pid,
                    None => continue,
                }
            } else {
                pid_of(hwnd)
            };
            let exe = match self.0.get(&pid) {
                Some(exe) => exe.clone(),
                None => match shell::process_image(pid) {
                    Some(exe) => {
                        self.0.insert(pid, exe.clone());
                        exe
                    }
                    None => continue,
                },
            };
            seen.insert(pid, ());
            windows.push(AppWindow { hwnd, exe, aumid: shell::window_aumid(hwnd) });
        }
        self.0.retain(|pid, _| seen.contains_key(pid));
        Snapshot { windows, foreground, covered, fullscreen }
    }
}
