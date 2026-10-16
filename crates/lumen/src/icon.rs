//! The Lumen tray icon, drawn at runtime: a black rounded-square tile with a centered orb.

use windows::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, GetSystemMetrics, LoadIconW, HICON, ICONINFO, IDI_APPLICATION, SM_CXSMICON};

pub fn create() -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    unsafe { build(size) }.unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() })
}

unsafe fn build(size: i32) -> Option<HICON> {
    let s = size as f32;
    let mut pixels = vec![0u32; (size * size) as usize];
    let radius = s * 0.24;
    let orb = s * 0.2;
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Signed distance to the rounded square, for a soft anti-aliased edge.
            let qx = (px - s / 2.0).abs() - (s / 2.0 - radius);
            let qy = (py - s / 2.0).abs() - (s / 2.0 - radius);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - radius;
            let tile = (0.5 - outside).clamp(0.0, 1.0);
            let d_orb = ((px - s / 2.0).powi(2) + (py - s / 2.0).powi(2)).sqrt() - orb;
            let white = (0.5 - d_orb).clamp(0.0, 1.0);
            let a = tile;
            let v = white * a; // premultiplied
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
