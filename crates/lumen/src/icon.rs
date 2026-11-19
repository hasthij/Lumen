//! The Lumen tray icon, drawn at runtime: a black rounded-square tile with the white Beam L mark.

use windows::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, GetSystemMetrics, LoadIconW, HICON, ICONINFO, IDI_APPLICATION, SM_CXSMICON};

pub fn create() -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    unsafe { build(size) }.unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() })
}

/// Signed distance to a rounded rect (x, y, w, h, r).
fn rrect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (px - (x + w / 2.0)).abs() - (w / 2.0 - r);
    let qy = (py - (y + h / 2.0)).abs() - (h / 2.0 - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// Beam L in a 64-unit box (same shape as scripts/make_icon.py and the island orb).
fn beam(x: f32, y: f32) -> bool {
    rrect(x, y, 15.0, 10.0, 9.5, 44.0, 4.75) <= 0.0
        || rrect(x, y, 15.0, 44.5, 34.0, 9.5, 4.75) <= 0.0
        || (x - 44.0).powi(2) + (y - 21.0).powi(2) <= 5.25f32.powi(2)
}

unsafe fn build(size: i32) -> Option<HICON> {
    let s = size as f32;
    let mut pixels = vec![0u32; (size * size) as usize];
    const SS: i32 = 4;
    for y in 0..size {
        for x in 0..size {
            let (mut tile, mut mark) = (0, 0);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if rrect(px, py, 0.0, 0.0, s, s, s * 0.24) <= 0.0 {
                        tile += 1;
                        if beam(px * 64.0 / s, py * 64.0 / s) {
                            mark += 1;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (a, v) = (tile as f32 / n, mark as f32 / n); // v is already premultiplied
            let (a8, v8) = ((a * 255.0) as u32, (v * 255.0) as u32);
            pixels[(y * size + x) as usize] = (a8 << 24) | (v8 << 16) | (v8 << 8) | v8;
        }
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
    let mask = CreateBitmap(size, size, 1, 1, None);
    let ii = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&ii).ok();
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}
