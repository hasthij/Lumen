//! Small bar at the top right of the screen shown while skins are being moved, with a Done
//! button. Never takes focus; a click on Done posts `WM_MOVE_DONE` to the app.

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clock::display_scale;
use crate::gfx::{Fonts, LayeredSurface};
use crate::rainmeter::skin::{Item, Scene, TextStyle};

const BAR_W: f32 = 300.0;
const BAR_H: f32 = 52.0;
const BTN_W: f32 = 84.0;
const MARGIN: f32 = 16.0;

pub struct MoveBar {
    hwnd: HWND,
    _surface: LayeredSurface,
}

impl MoveBar {
    pub fn show() -> Result<Self> {
        let hwnd = unsafe { create_window()? };
        let mut surface = LayeredSurface::new()?;
        let mut fonts = Fonts::new()?;
        let scale = display_scale();
        let origin = top_right(scale);
        surface.present(hwnd, &mut fonts, &build_scene(scale), 1.0, origin, 255)?;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        Ok(Self { hwnd, _surface: surface })
    }
}

impl Drop for MoveBar {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn top_right(scale: f32) -> POINT {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let wa = unsafe {
        let mon = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(mon, &mut info).as_bool() { info.rcWork } else { RECT { left: 0, top: 0, right: 1920, bottom: 1080 } }
    };
    POINT { x: wa.right - ((BAR_W + MARGIN) * scale) as i32, y: wa.top + (MARGIN * scale) as i32 }
}

fn build_scene(scale: f32) -> Scene {
    let s = |v: f32| v * scale;
    let text = |t: &str, size: f32, weight: u32, x: f32, y: f32, a: f32| Item::Text {
        text: t.into(),
        style: TextStyle { face: "Segoe UI".into(), size: s(size), weight, italic: false, spacing: [0.0; 3] },
        color: [1.0, 1.0, 1.0, a],
        x: s(x),
        y: s(y),
        stretch: 1.0,
    };
    let items = vec![
        Item::Rect { x: 0.0, y: 0.0, w: s(BAR_W), h: s(BAR_H), radius: s(12.0), fill: [0.08, 0.08, 0.09, 0.94], stroke: [1.0, 1.0, 1.0, 0.18], stroke_width: 1.0 },
        text("Moving skins", 13.0, 600, 16.0, 8.0, 0.95),
        text("Drag a skin. Hold Ctrl to snap", 11.0, 400, 16.0, 28.0, 0.6),
        Item::Rect { x: s(BAR_W - BTN_W - 10.0), y: s(10.0), w: s(BTN_W), h: s(BAR_H - 20.0), radius: s(8.0), fill: [1.0, 1.0, 1.0, 0.92], stroke: [0.0; 4], stroke_width: 0.0 },
        Item::Text {
            text: "Done".into(),
            style: TextStyle { face: "Segoe UI".into(), size: s(13.0), weight: 600, italic: false, spacing: [0.0; 3] },
            color: [0.05, 0.05, 0.06, 1.0],
            x: s(BAR_W - BTN_W - 10.0 + 24.0),
            y: s(14.0),
            stretch: 1.0,
        },
    ];
    Scene { items, width: s(BAR_W).ceil() as u32, height: s(BAR_H).ceil() as u32 }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONUP => {
            // Anywhere on the bar finishes; the Done button is just the obvious target.
            if let Ok(app) = FindWindowW(w!("LumenApp"), None) {
                let _ = PostMessageW(app, crate::app::WM_MOVE_DONE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_HAND)?,
        lpszClassName: w!("LumenMoveBar"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
    CreateWindowExW(ex, w!("LumenMoveBar"), w!("Lumen Move"), WS_POPUP, 0, 0, 1, 1, None, None, instance, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_matches_bar_size_and_has_a_done_label() {
        let s = build_scene(1.5);
        assert_eq!((s.width, s.height), (450, 78));
        assert!(s.items.iter().any(|i| matches!(i, Item::Text { text, .. } if text == "Done")));
    }
}
