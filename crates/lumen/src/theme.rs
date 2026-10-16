//! Themes: a colour tint laid over a Windows backdrop.
//!
//! Mica is the default because it is nearly free: Windows samples the wallpaper once
//! instead of blurring whatever is behind the window every frame like acrylic does.

use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::UI::Controls::MARGINS;

use crate::config::Appearance;

pub const PRESETS: &[(&str, &str, u32, u32)] = &[
    // (id, name, tint, accent)
    ("graphite", "Graphite", 0x202022, 0),
    ("midnight", "Midnight", 0x0E1830, 0x6EA8FF),
    ("violet", "Violet", 0x1C1430, 0xB18CFF),
    ("rose", "Rose", 0x2A121D, 0xFF8FB3),
    ("forest", "Forest", 0x0F2419, 0x6FD39A),
    ("amber", "Amber", 0x2A1C0B, 0xFFBF5E),
    ("ocean", "Ocean", 0x0A2226, 0x5ED4E0),
    ("accent", "Windows accent", 0, 0),
];

pub const BACKDROPS: &[(&str, &str)] = &[("mica", "Mica"), ("mica_alt", "Mica Alt"), ("acrylic", "Acrylic"), ("solid", "Solid")];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub tint: [f32; 3],
    /// How strongly the tint covers the backdrop (0 = pure Mica, 1 = solid colour).
    pub strength: f32,
    pub accent: [f32; 3],
    pub solid: bool,
}

impl Default for Theme {
    fn default() -> Self {
        resolve(&Appearance::default())
    }
}

pub fn rgb(hex: u32) -> [f32; 3] {
    [((hex >> 16) & 0xFF) as f32 / 255.0, ((hex >> 8) & 0xFF) as f32 / 255.0, (hex & 0xFF) as f32 / 255.0]
}

pub fn to_hex(c: [f32; 3]) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    format!("#{:02X}{:02X}{:02X}", b(c[0]), b(c[1]), b(c[2]))
}

pub fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let h = s.trim().trim_start_matches('#');
    (h.len() == 6).then(|| u32::from_str_radix(h, 16).ok().map(rgb)).flatten()
}

/// HSV (h in degrees, s and v 0-1) to RGB.
pub fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

/// RGB to HSV (h in degrees, s and v 0-1).
pub fn to_hsv(c: [f32; 3]) -> (f32, f32, f32) {
    let max = c[0].max(c[1]).max(c[2]);
    let min = c[0].min(c[1]).min(c[2]);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == c[0] {
        60.0 * ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if max == c[1] {
        60.0 * ((c[2] - c[0]) / d + 2.0)
    } else {
        60.0 * ((c[0] - c[1]) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

/// The Windows accent colour.
pub fn system_accent() -> [f32; 3] {
    let mut argb = 0u32;
    let mut opaque = BOOL(0);
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.is_ok() {
        rgb(argb & 0xFFFFFF)
    } else {
        rgb(0x0078D4)
    }
}

fn lighten(c: [f32; 3], t: f32) -> [f32; 3] {
    [c[0] + (1.0 - c[0]) * t, c[1] + (1.0 - c[1]) * t, c[2] + (1.0 - c[2]) * t]
}

fn darken(c: [f32; 3], t: f32) -> [f32; 3] {
    [c[0] * t, c[1] * t, c[2] * t]
}

pub fn resolve(a: &Appearance) -> Theme {
    let sys = system_accent();
    let (tint, accent) = match a.theme.as_str() {
        "custom" => {
            let t = parse_hex(&a.tint).unwrap_or(rgb(0x202022));
            (t, lighten(t, 0.55))
        }
        "accent" => (darken(sys, 0.22), lighten(sys, 0.4)),
        id => match PRESETS.iter().find(|p| p.0 == id) {
            Some(&(_, _, tint, 0)) => (rgb(tint), lighten(sys, 0.4)),
            Some(&(_, _, tint, accent)) => (rgb(tint), rgb(accent)),
            None => (rgb(0x202022), lighten(sys, 0.4)),
        },
    };
    Theme { tint, strength: a.strength.clamp(0.0, 1.0), accent, solid: a.backdrop == "solid" }
}

/// Applies dark mode, rounded corners and the chosen backdrop to a window whose frame is
/// extended into the client area. Returns false when the backdrop is unavailable (the
/// caller then draws a solid background).
pub fn apply_backdrop(hwnd: HWND, a: &Appearance, popup: bool) -> bool {
    unsafe {
        let on = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as *const _, 4);
        if popup {
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        }
        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let kind = match a.backdrop.as_str() {
            "mica_alt" => DWMSBT_TABBEDWINDOW,
            "acrylic" => DWMSBT_TRANSIENTWINDOW,
            "solid" => DWMSBT_NONE,
            _ => DWMSBT_MAINWINDOW,
        };
        DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &kind as *const _ as *const _, 4).is_ok() && a.backdrop != "solid"
    }
}
