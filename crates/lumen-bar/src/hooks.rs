//! Event sources. Nothing here polls: window changes arrive as WinEvents, the cursor via a
//! low-level mouse hook on its own thread (so a busy UI thread can never stall the mouse),
//! and config edits via a directory change notification.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering::Relaxed};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_LAST_WRITE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
pub const WM_APP_MOUSE: u32 = WM_APP + 2;
pub const WM_APP_CONFIG: u32 = WM_APP + 3;

/// Bits in WM_APP_MOUSE's wParam.
pub const MOUSE_AT_EDGE: usize = 1;
pub const MOUSE_IN_DOCK: usize = 2;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

fn post(msg: u32, wparam: usize) {
    let hwnd = HWND(TARGET.load(Relaxed) as _);
    unsafe {
        let _ = PostMessageW(hwnd, msg, WPARAM(wparam), LPARAM(0));
    }
}

/// Called by the UI thread once it has handled WM_APP_REFRESH, so the next event posts again.
pub fn refresh_handled() {
    REFRESH_PENDING.store(false, Relaxed);
}

/// Asks for a window refresh; many events in a burst collapse into one message.
pub fn request_refresh() {
    if !REFRESH_PENDING.swap(true, Relaxed) {
        post(WM_APP_REFRESH, 0);
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND && (id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32) {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE && GetAncestor(hwnd, GA_ROOT) != hwnd {
        return;
    }
    request_refresh();
}

pub fn install(target: HWND) {
    TARGET.store(target.0 as isize, Relaxed);
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    for (min, max) in ranges {
        unsafe {
            SetWinEventHook(min, max, None, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        }
    }
    std::thread::spawn(mouse_thread);
    std::thread::spawn(config_thread);
}

// ---- cursor ---------------------------------------------------------------------------------

/// Geometry the mouse hook tests against, in physical pixels. Written by the UI thread.
pub struct Zone {
    mon_left: AtomicI32,
    mon_right: AtomicI32,
    mon_bottom: AtomicI32,
    dock: [AtomicI32; 4],
}

pub static ZONE: Zone = Zone {
    mon_left: AtomicI32::new(0),
    mon_right: AtomicI32::new(0),
    mon_bottom: AtomicI32::new(0),
    dock: [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)],
};
static MOUSE_STATE: AtomicU32 = AtomicU32::new(0);

impl Zone {
    pub fn set(&self, mon: (i32, i32, i32), dock: (i32, i32, i32, i32)) {
        self.mon_left.store(mon.0, Relaxed);
        self.mon_right.store(mon.1, Relaxed);
        self.mon_bottom.store(mon.2, Relaxed);
        for (slot, v) in self.dock.iter().zip([dock.0, dock.1, dock.2, dock.3]) {
            slot.store(v, Relaxed);
        }
    }

    fn classify(&self, x: i32, y: i32) -> usize {
        let [l, t, r, _] = self.dock.each_ref().map(|a| a.load(Relaxed));
        let bottom = self.mon_bottom.load(Relaxed);
        let on_monitor = x >= self.mon_left.load(Relaxed) && x < self.mon_right.load(Relaxed);
        let mut bits = 0;
        // The trigger is the bottom pixel row under the dock, so the rest of the edge stays
        // free for the native taskbar.
        if on_monitor && y >= bottom - 1 && x >= l && x < r {
            bits |= MOUSE_AT_EDGE;
        }
        // Anything from just above the dock down to the screen edge counts as "in the dock",
        // so crossing the native taskbar on the way up doesn't hide it.
        let slack = 16;
        if x >= l - slack && x < r + slack && y >= t - slack && y <= bottom {
            bits |= MOUSE_IN_DOCK;
        }
        bits
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_MOUSEMOVE {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let bits = ZONE.classify(info.pt.x, info.pt.y) as u32;
        if MOUSE_STATE.swap(bits, Relaxed) != bits {
            post(WM_APP_MOUSE, bits as usize);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn mouse_thread() {
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        if SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0).is_err() {
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
    }
}

// ---- config file ----------------------------------------------------------------------------

fn config_thread() {
    let dir = windows::core::HSTRING::from(crate::config::dir().as_os_str());
    unsafe {
        let Ok(handle) = FindFirstChangeNotificationW(&dir, false, FILE_NOTIFY_CHANGE_LAST_WRITE) else {
            return;
        };
        loop {
            WaitForSingleObject(handle, INFINITE);
            post(WM_APP_CONFIG, 0);
            if FindNextChangeNotification(handle).is_err() {
                return;
            }
        }
    }
}
