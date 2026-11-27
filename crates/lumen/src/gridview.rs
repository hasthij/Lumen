//! Faint grid drawn over the desktop while skins are being moved, matching the
//! snapping done in `clock::snap_axis`. Click-through, never activates.

use windows::core::{w, Result};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clock::{display_scale, EDGE_MARGIN, GRID};
use crate::gfx::{Fonts, LayeredSurface};
use crate::rainmeter::skin::{Item, Scene};

pub struct GridOverlay {
    hwnd: HWND,
    surface: LayeredSurface,
    fonts: Fonts,
}

impl GridOverlay {
    pub fn show() -> Result<Self> {
        let hwnd = unsafe { create_window()? };
        let mut me = Self { hwnd, surface: LayeredSurface::new()?, fonts: Fonts::new()? };
        let (origin, scene) = build_scene(&work_areas(), display_scale());
        me.surface.present(hwnd, &mut me.fonts, &scene, 1.0, origin, 255)?;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        Ok(me)
    }
}

impl Drop for GridOverlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("LumenGrid"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
    CreateWindowExW(ex, w!("LumenGrid"), w!("Lumen Grid"), WS_POPUP, 0, 0, 1, 1, None, None, instance, None)
}

fn work_areas() -> Vec<RECT> {
    unsafe extern "system" fn each(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<RECT>);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            out.push(info.rcWork);
        }
        BOOL(1)
    }
    let mut out: Vec<RECT> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(HDC::default(), None, Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// Scene in physical pixels covering every work area, plus the screen position of its top-left.
/// Lines match `snap_axis`: every GRID px from EDGE_MARGIN inside the work area, with brighter
/// center lines.
fn build_scene(areas: &[RECT], scale: f32) -> (POINT, Scene) {
    let left = areas.iter().map(|a| a.left).min().unwrap_or(0);
    let top = areas.iter().map(|a| a.top).min().unwrap_or(0);
    let right = areas.iter().map(|a| a.right).max().unwrap_or(1);
    let bottom = areas.iter().map(|a| a.bottom).max().unwrap_or(1);
    let (m, g) = ((EDGE_MARGIN * scale).round(), (GRID * scale).round().max(1.0));
    let faint = [1.0, 1.0, 1.0, 0.13];
    let strong = [1.0, 1.0, 1.0, 0.38];
    let line = |x: f32, y: f32, w: f32, h: f32, fill| Item::Rect { x, y, w, h, radius: 0.0, fill, stroke: [0.0; 4], stroke_width: 0.0 };
    let mut items = Vec::new();
    for a in areas {
        let (x0, y0) = ((a.left - left) as f32, (a.top - top) as f32);
        let (w, h) = ((a.right - a.left) as f32, (a.bottom - a.top) as f32);
        let mut x = m;
        while x <= w - m {
            items.push(line(x0 + x, y0 + m, 1.0, h - 2.0 * m, faint));
            x += g;
        }
        let mut y = m;
        while y <= h - m {
            items.push(line(x0 + m, y0 + y, w - 2.0 * m, 1.0, faint));
            y += g;
        }
        items.push(line(x0 + (w / 2.0).floor(), y0, 1.0, h, strong));
        items.push(line(x0, y0 + (h / 2.0).floor(), w, 1.0, strong));
        items.push(Item::Rect { x: x0 + m, y: y0 + m, w: w - 2.0 * m, h: h - 2.0 * m, radius: 0.0, fill: [0.0; 4], stroke: strong, stroke_width: 1.0 });
    }
    (POINT { x: left, y: top }, Scene { items, width: (right - left) as u32, height: (bottom - top) as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_spans_both_monitors_and_has_center_lines() {
        let a = [RECT { left: 0, top: 0, right: 1920, bottom: 1040 }, RECT { left: 1920, top: 0, right: 3840, bottom: 1040 }];
        let (origin, scene) = build_scene(&a, 1.0);
        assert_eq!((origin.x, origin.y, scene.width, scene.height), (0, 0, 3840, 1040));
        assert!(scene.items.len() > 100);
    }
}
