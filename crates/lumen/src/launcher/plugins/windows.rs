//! Window walker: switch to (or close) any open window by title or app name.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::launcher::fuzzy;
use crate::launcher::item::{Action, Icon, Item, Kind};
use crate::launcher::shell::process_image;

pub struct OpenWindow {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
}

/// Taken once when the launcher opens; switching windows doesn't need live data.
pub fn snapshot(own: HWND) -> Vec<OpenWindow> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
        (*(lp.0 as *mut Vec<HWND>)).push(hwnd);
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    let mut out = Vec::new();
    for hwnd in all {
        unsafe {
            if hwnd == own || !IsWindowVisible(hwnd).as_bool() {
                continue;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 {
                continue;
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let app_window = ex & WS_EX_APPWINDOW.0 != 0;
            if (ex & WS_EX_TOOLWINDOW.0 != 0 || ex & WS_EX_NOACTIVATE.0 != 0) && !app_window {
                continue;
            }
            if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) && !app_window {
                continue;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            let title = String::from_utf16_lossy(&buf[..n]);
            if title == "Program Manager" {
                continue;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let exe = process_image(pid).unwrap_or_default();
            out.push(OpenWindow { hwnd: hwnd.0 as isize, title, exe });
        }
    }
    out
}

pub fn query(q: &str, explicit: bool, windows: &[OpenWindow], out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    if q.len() < 2 && !explicit {
        return;
    }
    for w in windows {
        let exe_name = std::path::Path::new(&w.exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let s = if q.is_empty() {
            Some(0)
        } else {
            fuzzy::score(&q, &w.title).max(fuzzy::score(&q, &exe_name).map(|s| s - 100))
        };
        let Some(s) = s else { continue };
        let icon = if w.exe.is_empty() { Icon::Glyph } else { Icon::Shell(w.exe.clone()) };
        out.push(
            Item::new(Kind::Window, w.title.clone(), format!("Switch to window  ·  {exe_name}"), Action::SwitchTo(w.hwnd))
                .icon(icon)
                .key(format!("window:{}", w.hwnd))
                // Below apps of the same name: "chrome" should launch Chrome first.
                .score(s - 300)
                .with(Action::CloseWindow(w.hwnd)),
        );
    }
}
