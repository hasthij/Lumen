//! lumen-bar: a macOS-style dock for Windows 11.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod dock;
mod gfx;
mod hooks;
mod shell;
mod winlist;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

fn main() -> windows::core::Result<()> {
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\LumenDock"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }

    let hwnd = dock::Dock::create()?;
    hooks::install(hwnd);
    hooks::request_refresh();

    // Block in GetMessage while idle; only while the dock slides do we run a frame loop,
    // paced by the compositor (DwmFlush waits for the next vsync).
    let mut msg = MSG::default();
    loop {
        let animating = dock::DOCK.with(|d| d.borrow().as_ref().is_some_and(|d| d.is_animating()));
        unsafe {
            if animating {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                dock::DOCK.with(|d| d.borrow_mut().as_mut().map(|d| d.tick()));
                let _ = DwmFlush();
            } else {
                if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
