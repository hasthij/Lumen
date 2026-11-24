//! Island: an edge-docked, hidden-by-default pill that grows into a panel with the clipboard
//! history and a media player. One layered window, redrawn only while it moves or changes.
//! The module is created and dropped by the app, so it costs nothing while turned off.

mod clip;
mod media;
mod mixer;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::DataExchange::{AddClipboardFormatListener, GetClipboardSequenceNumber, RemoveClipboardFormatListener};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SendInput, SetCapture, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_LBUTTON, VK_V,
};

/// Ctrl+V to whatever app has focus (the island never takes focus).
/// Runs on a short delay off the UI thread so the click finishes and the clipboard settles first;
/// keys carry scan codes because many apps ignore virtual-key-only input.
fn send_paste() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(90));
        let key = |vk: VIRTUAL_KEY, scan: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
            },
        };
        let seq = [key(VK_CONTROL, 0x1D, false), key(VK_V, 0x2F, false), key(VK_V, 0x2F, true), key(VK_CONTROL, 0x1D, true)];
        unsafe { SendInput(&seq, std::mem::size_of::<INPUT>() as i32) };
    });
}
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, ShellExecuteW, HDROP, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::IslandConfig;

const WM_MEDIA: u32 = WM_APP + 1;
const WM_FRAME: u32 = WM_APP + 2;
static FAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FRAME_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// File-picker result (shelf?, paths), handed back from the dialog thread via WM_PICKED.
const WM_PICKED: u32 = WM_APP + 3;
static PICKED: std::sync::Mutex<Option<(bool, Vec<String>)>> = std::sync::Mutex::new(None);
static PICKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Brightness read back from the DDC/CI thread (WM_BRI); setter queue + busy flag (DDC/CI is slow).
const WM_BRI: u32 = WM_APP + 4;
static BRI_READ: std::sync::Mutex<Option<Vec<Option<f32>>>> = std::sync::Mutex::new(None);
static BRI_SET: std::sync::Mutex<Option<(usize, f32)>> = std::sync::Mutex::new(None);
static BRI_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Installed apps (name, path to add) for the Launch picker, scanned off the UI thread (WM_APPS).
const WM_APPS: u32 = WM_APP + 5;
static APPS: std::sync::Mutex<Option<Vec<(String, String)>>> = std::sync::Mutex::new(None);
const PICK_Y: f32 = 96.0;
const PICK_H: f32 = 40.0;

fn scan_apps(hwnd: HWND) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let v = crate::launcher::apps::scan().into_iter().map(|a| (a.name, a.path.unwrap_or(a.launch))).collect();
        *APPS.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
        let _ = PostMessageW(HWND(h as *mut _), WM_APPS, WPARAM(0), LPARAM(0));
    });
}

fn read_brightness(hwnd: HWND) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let v = mixer::brightness_all();
        *BRI_READ.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
        let _ = PostMessageW(HWND(h as *mut _), WM_BRI, WPARAM(0), LPARAM(0));
    });
}

/// Mixer snapshot (apps, master) sampled off the UI thread (WM_MIX); busy flag skips overlapping polls.
const WM_MIX: u32 = WM_APP + 6;
static MIX: std::sync::Mutex<Option<(Vec<mixer::Chan>, Option<(f32, bool)>)>> = std::sync::Mutex::new(None);
static MIX_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sample_mix_async(hwnd: HWND, pins: Vec<String>) {
    if MIX_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let h = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let snap = (mixer::apps(&pins), mixer::master());
        *MIX.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
        MIX_BUSY.store(false, Ordering::Release);
        let _ = PostMessageW(HWND(h as *mut _), WM_MIX, WPARAM(0), LPARAM(0));
    });
}

/// Pin candidates for the mixer "+": every app with an audio session, listed off the UI thread (WM_APPS).
fn scan_sessions(hwnd: HWND) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        *APPS.lock().unwrap_or_else(|e| e.into_inner()) = Some(mixer::sessions());
        let _ = PostMessageW(HWND(h as *mut _), WM_APPS, WPARAM(0), LPARAM(0));
    });
}

/// Queues an absolute brightness for monitor `i`; one worker drains the latest value so drags never pile up.
fn push_brightness(i: usize, v: f32) {
    *BRI_SET.lock().unwrap_or_else(|e| e.into_inner()) = Some((i, v));
    if BRI_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        drain_brightness()
    });
}

fn drain_brightness() {
    loop {
        let next = BRI_SET.lock().unwrap_or_else(|e| e.into_inner()).take();
        match next {
            Some((i, v)) => {
                mixer::brightness(i, Some(v));
            }
            None => {
                BRI_BUSY.store(false, Ordering::Release);
                // A value queued between take() and store() would be stranded: reclaim it.
                if BRI_SET.lock().unwrap_or_else(|e| e.into_inner()).is_none() || BRI_BUSY.swap(true, Ordering::AcqRel) {
                    return;
                }
            }
        }
    }
}
const T_TICK: usize = 1;
const T_CLIP: usize = 2;

// Design units (scaled by DPI x the user's scale).
// Content box: wide and short on the top edge, narrow and tall on the sides. Same for both tabs.
const TOP_BOX: (f32, f32) = (900.0, 270.0);
const TOP_MIN_W: f32 = 450.0; // top dock width of a player unless its text needs more
const TOP_MAX_W: f32 = 640.0; // long titles grow the bar up to here, then ellipsize
const SIDE_BOX: (f32, f32) = (360.0, 600.0);
const F: f32 = 14.0; // fillet margin along the docked edge
const PEEK: f32 = 0.28;
const PEEK_EXT: f32 = 42.0;
const PEEK_HH: f32 = 21.0;
// Tiles for other media sessions: side by side on the top edge, stacked on the sides.
const MAX_TILES: usize = 3;
const TILE_W: f32 = 200.0; // top dock
const TILE_H: f32 = 72.0; // side docks
const TILE_GAP: f32 = 14.0;
const SIDE_TILES_Y: f32 = 452.0;
// Several playing sessions share the panel: equal players side by side (top) or stacked, compact (sides).
const MAX_PLAYERS: usize = 3;
const MULTI_W: f32 = 560.0; // top dock, per player when more than one
const MULTI_H: f32 = 172.0; // side docks, per player when more than one
const MULTI_Y: f32 = 58.0;
// Tuned list/button metrics (design units).
const ROW_H: f32 = 42.0;
const ROW_GAP: f32 = 6.0;
const ROW_PITCH: f32 = ROW_H + ROW_GAP;
const BTN: f32 = 40.0;
const BTN_R: f32 = 7.0;

const PAD: f32 = 26.0;
/// Sound tab geometry: track top, column width/gap, mute button size, brightness pill width.
const SND_Y: f32 = 64.0;
const COL_W: f32 = 56.0;
const COL_GAP: f32 = 6.0;
const MUTE_D: f32 = 28.0;
const BRI_W: f32 = 64.0;
const BRI_GAP: f32 = 10.0;
const TAB_GAP: f32 = 4.0;
const TAB_R: f32 = 16.0;
const PRESS: f32 = 0.95;
const LIFT: f32 = 1.5;
const CHIP_Y: f32 = 56.0;
const CHIP_H: f32 = 24.0;
const PIN_X: f32 = 64.0; // button x measured from the right edge of the row
const DEL_X: f32 = 34.0;
const LIST_Y: f32 = 88.0; // list top: under the tab bar and the filter strip

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Clip,
    Media,
    Sound,
    Launch,
    System,
    Shelf,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Clip => "Clip",
            Tab::Media => "Media",
            Tab::Sound => "Sound",
            Tab::Launch => "Launch",
            Tab::System => "System",
            Tab::Shelf => "Notes",
        }
    }
    fn glyph(self) -> &'static str {
        match self {
            Tab::Clip => "\u{E77F}",
            Tab::Media => "\u{E8D6}",
            Tab::Sound => "\u{E767}",
            Tab::Launch => "\u{ECAA}",
            Tab::System => "\u{E9D9}",
            Tab::Shelf => "\u{E70B}",
        }
    }
    fn width(self) -> f32 {
        match self {
            Tab::Clip => 46.0,
            Tab::Media => 58.0,
            Tab::Sound => 56.0,
            Tab::Launch => 62.0,
            Tab::System => 64.0,
            Tab::Shelf => 54.0,
        }
    }
}

/// Media-tab layout in content coordinates.
#[derive(Clone, Copy)]
struct Lay {
    art: (f32, f32, f32),
    tx: f32,
    tw: f32,
    ty: f32,
    sy: f32,
    cy: f32,
    cxs: [f32; 3],
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
    Top,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Hidden,
    Peek,
    Open,
}

#[derive(Clone, Copy, PartialEq)]
enum Hover {
    None,
    Row(usize),
    Tab(Tab),
    Sec(usize),
    Prev(usize),
    Play(usize),
    Next(usize),
    Clear,
    Scrub(usize),
    Pin(usize),
    Del(usize),
    Chip(u8),
    Item(usize),
    Add,
    /// Sound tab: slider / mute of channel n (0 = master, i + 1 = app i), brightness pill.
    Vol(usize),
    Mute(usize),
    Bri(usize),
    /// Launch app picker: row i of the installed-app list, back button.
    App(usize),
    Back,
}

#[derive(Clone)]
struct Fonts {
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_r: IDWriteTextFormat,
    center: IDWriteTextFormat,
    /// Centred bold figure inside the System gauges.
    num: IDWriteTextFormat,
    hdr: IDWriteTextFormat,
    icon: IDWriteTextFormat,
    icon_big: IDWriteTextFormat,
}

unsafe fn mk(dw: &IDWriteFactory, family: PCWSTR, size: f32, weight: i32, align: DWRITE_TEXT_ALIGNMENT) -> Result<IDWriteTextFormat> {
    let f = dw.CreateTextFormat(family, None, DWRITE_FONT_WEIGHT(weight), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))?;
    f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    f.SetTextAlignment(align)?;
    let sign = dw.CreateEllipsisTrimmingSign(&f)?;
    let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
    f.SetTrimming(&trimming, &sign)?;
    Ok(f)
}

impl Fonts {
    unsafe fn new(dw: &IDWriteFactory, family: &str, size: f32) -> Result<Fonts> {
        let fam = windows::core::HSTRING::from(family);
        let ui = PCWSTR(fam.as_ptr());
        let k = size / 13.0;
        let ic = w!("Segoe MDL2 Assets");
        let l = DWRITE_TEXT_ALIGNMENT_LEADING;
        let c = DWRITE_TEXT_ALIGNMENT_CENTER;
        Ok(Fonts {
            title: mk(dw, ui, 15.0 * k, 600, l)?,
            body: mk(dw, ui, 13.0 * k, 400, l)?,
            small: mk(dw, ui, 11.0 * k, 400, l)?,
            small_r: mk(dw, ui, 11.0 * k, 400, DWRITE_TEXT_ALIGNMENT_TRAILING)?,
            center: mk(dw, ui, 12.0 * k, 400, c)?,
            num: mk(dw, ui, 15.0 * k, 600, c)?,
            hdr: mk(dw, ui, 12.0 * k, 600, l)?,
            icon: mk(dw, ic, 15.0, 400, c)?,
            icon_big: mk(dw, ic, 22.0, 400, c)?,
        })
    }
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn hsl(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

/// Three box-blur passes (radius 2) each way, then saturation, on BGRA; output is opaque.
pub(crate) fn blur_sat(px: &mut [u8], w: usize, h: usize, sat: f32) {
    fn pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, horiz: bool) {
        let (n, m) = if horiz { (h, w) } else { (w, h) };
        for line in 0..n {
            let at = |i: usize| if horiz { (line * w + i) * 4 } else { (i * w + line) * 4 };
            for ch in 0..3 {
                let mut acc: i32 = 0;
                for j in -2i32..=2 {
                    acc += src[at(j.clamp(0, m as i32 - 1) as usize) + ch] as i32;
                }
                for i in 0..m {
                    dst[at(i) + ch] = (acc / 5) as u8;
                    let add = src[at((i + 3).min(m - 1)) + ch] as i32;
                    let sub = src[at(i.saturating_sub(2)) + ch] as i32;
                    acc += add - sub;
                }
            }
        }
    }
    let mut tmp = vec![0u8; px.len()];
    for _ in 0..3 {
        pass(px, &mut tmp, w, h, true);
        pass(&tmp, px, w, h, false);
    }
    for p in px.chunks_mut(4) {
        let (b, g, r) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let lum = 0.299 * r + 0.587 * g + 0.114 * b;
        p[0] = (lum + (b - lum) * sat).clamp(0.0, 255.0) as u8;
        p[1] = (lum + (g - lum) * sat).clamp(0.0, 255.0) as u8;
        p[2] = (lum + (r - lum) * sat).clamp(0.0, 255.0) as u8;
        p[3] = 255;
    }
}

fn parse_rgb(s: &str) -> (f32, f32, f32) {
    let v = u32::from_str_radix(s.trim().trim_start_matches('#'), 16).unwrap_or(0);
    (((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0)
}

fn parse_side(s: &str) -> Side {
    match s {
        "left" => Side::Left,
        "top" => Side::Top,
        _ => Side::Right,
    }
}

fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn age(t: u64) -> String {
    let d = clip::now().saturating_sub(t);
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

fn size_label(e: &clip::Entry) -> String {
    let b = e.size;
    match b {
        0 => String::new(),
        1..=1023 => format!("{b} B"),
        1024..=1048575 => format!("{:.0} KB", b as f64 / 1024.0),
        1048576..=1073741823 => format!("{:.1} MB", b as f64 / 1048576.0),
        _ => format!("{:.1} GB", b as f64 / 1073741824.0),
    }
}

fn row_label(e: &clip::Entry) -> String {
    if e.img {
        "Image".into()
    } else if e.files {
        let names: Vec<&str> = e.s.lines().map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p)).collect();
        match names.len() {
            0 => String::new(),
            1 => names[0].to_string(),
            n => format!("{} +{}", names[0], n - 1),
        }
    } else {
        e.s.chars().take(200).map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
    }
}

/// One System-tab sample.
#[derive(Clone, Copy)]
struct Sys {
    cpu: f32,
    ram: f32,
    ram_used: u64,
    ram_total: u64,
    /// (percent, charging); None on desktops without a battery.
    bat: Option<(u8, bool)>,
    disk_free: u64,
    disk_total: u64,
    /// Uptime second the disk was last queried.
    disk_at: u64,
    uptime_s: u64,
}

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / 1073741824.0)
}

/// Quick-launch paths, mixer pins (exe stems) and sticky notes, kept beside the config.
#[derive(serde::Serialize, serde::Deserialize)]
struct Items {
    #[serde(default)]
    launch: Vec<String>,
    #[serde(default = "default_pins")]
    mix_pins: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
}

fn default_pins() -> Vec<String> {
    vec!["spotify".into(), "discord".into()]
}

impl Default for Items {
    fn default() -> Items {
        Items { launch: Vec::new(), mix_pins: default_pins(), notes: Vec::new() }
    }
}

impl Items {
    fn path() -> std::path::PathBuf {
        let mut p = crate::config::path();
        p.set_file_name("island_items.toml");
        p
    }
    fn load() -> Items {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(s) = toml::to_string(self) {
            let _ = std::fs::write(Self::path(), s);
        }
    }
    /// Adds launch paths (deduped, newest first) and caps the list.
    fn add(&mut self, paths: Vec<String>) {
        let v = &mut self.launch;
        for p in paths {
            v.retain(|x| !x.eq_ignore_ascii_case(&p));
            v.insert(0, p);
        }
        v.truncate(12);
        self.save();
    }
}

/// Shelf/Launch grid tile size and pitch.
const TILE: f32 = 72.0;
const TILE_P: f32 = 80.0;

fn open_path(p: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &windows::core::HSTRING::from(p), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn file_name(p: &str) -> &str {
    let t = p.trim_end_matches(['\\', '/']);
    let n = t.rsplit(['\\', '/']).next().unwrap_or(t);
    let n = if n.is_empty() { p } else { n };
    match n.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && matches!(ext.to_ascii_lowercase().as_str(), "lnk" | "exe" | "url") => stem,
        _ => n,
    }
}

/// Drawing helper: panel design coordinates, one reusable brush.
struct Ctx<'a> {
    rt: &'a ID2D1DCRenderTarget,
    br: &'a ID2D1SolidColorBrush,
    fg: (f32, f32, f32),
    /// Cover accent colour while the Spotify tint is on.
    acc: Option<(f32, f32, f32)>,
    alpha: f32,
    ox: f32,
    oy: f32,
}

impl Ctx<'_> {
    unsafe fn col(&self, a: f32) {
        self.br.SetColor(&color(self.fg.0, self.fg.1, self.fg.2, a * self.alpha));
    }
    /// `rrect` in the cover accent when there is one.
    unsafe fn rrect_acc(&self, x: f32, y: f32, w: f32, h: f32, r: f32, a: f32) {
        let Some(c) = self.acc else { return self.rrect(x, y, w, h, r, a) };
        self.br.SetColor(&color(c.0, c.1, c.2, a * self.alpha));
        self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rect(self.ox + x, self.oy + y, w, h), radiusX: r, radiusY: r }, self.br);
    }
    unsafe fn circle_acc(&self, x: f32, y: f32, r: f32, a: f32) {
        let Some(c) = self.acc else { return self.circle(x, y, r, a) };
        self.br.SetColor(&color(c.0, c.1, c.2, a * self.alpha));
        self.rt.FillEllipse(&D2D1_ELLIPSE { point: D2D_POINT_2F { x: self.ox + x, y: self.oy + y }, radiusX: r, radiusY: r }, self.br);
    }
    unsafe fn text(&self, s: &str, f: &IDWriteTextFormat, x: f32, y: f32, w: f32, h: f32, a: f32) {
        self.col(a);
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.rt.DrawText(&wide, f, &rect(self.ox + x, self.oy + y, w, h), self.br, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
    }
    unsafe fn rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, a: f32) {
        self.col(a);
        let rr = D2D1_ROUNDED_RECT { rect: rect(self.ox + x, self.oy + y, w, h), radiusX: r, radiusY: r };
        self.rt.FillRoundedRectangle(&rr, self.br);
    }
    /// Dark text for use on a light filled shape (e.g. the play button).
    unsafe fn ink(&self, s: &str, f: &IDWriteTextFormat, x: f32, y: f32, w: f32, h: f32, a: f32) {
        self.br.SetColor(&color(0.04, 0.04, 0.06, a * self.alpha));
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.rt.DrawText(&wide, f, &rect(self.ox + x, self.oy + y, w, h), self.br, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
    }
    /// Vector transport icon (0 play, 1 pause, 2 previous, 3 next) of half-size `s` centred on (x, y).
    unsafe fn icon(&self, kind: u8, x: f32, y: f32, s: f32, dark: bool, a: f32) {
        if dark {
            self.br.SetColor(&color(0.04, 0.04, 0.06, a * self.alpha));
        } else {
            self.col(a);
        }
        let tri = |p: [(f32, f32); 3]| {
            let Ok(f) = self.rt.GetFactory().and_then(|f| f.CreatePathGeometry()) else { return };
            let Ok(sink) = f.Open() else { return };
            let pt = |(px, py): (f32, f32)| D2D_POINT_2F { x: self.ox + px, y: self.oy + py };
            sink.BeginFigure(pt(p[0]), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(pt(p[1]));
            sink.AddLine(pt(p[2]));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            self.rt.FillGeometry(&f, self.br, None);
        };
        let bar = |bx: f32, bw: f32| {
            let rr = D2D1_ROUNDED_RECT { rect: rect(self.ox + bx, self.oy + y - s * 0.5, bw, s), radiusX: bw * 0.3, radiusY: bw * 0.3 };
            self.rt.FillRoundedRectangle(&rr, self.br);
        };
        match kind {
            0 => tri([(x - 0.3 * s, y - 0.55 * s), (x - 0.3 * s, y + 0.55 * s), (x + 0.55 * s, y)]),
            1 => {
                bar(x - 0.45 * s, 0.34 * s);
                bar(x + 0.11 * s, 0.34 * s);
            }
            2 => {
                bar(x - 0.55 * s, 0.17 * s);
                tri([(x + 0.55 * s, y - 0.5 * s), (x + 0.55 * s, y + 0.5 * s), (x - 0.3 * s, y)]);
            }
            _ => {
                bar(x + 0.38 * s, 0.17 * s);
                tri([(x - 0.55 * s, y - 0.5 * s), (x - 0.55 * s, y + 0.5 * s), (x + 0.3 * s, y)]);
            }
        }
    }
    /// Donut gauge: faint full track plus a clockwise arc from 12 o'clock covering `f` (0..1), square-ended.
    unsafe fn ring(&self, x: f32, y: f32, r: f32, w: f32, f: f32, a: f32, warn: bool) {
        let c = D2D_POINT_2F { x: self.ox + x, y: self.oy + y };
        self.col(0.12);
        self.rt.DrawEllipse(&D2D1_ELLIPSE { point: c, radiusX: r, radiusY: r }, self.br, w, None);
        let f = f.clamp(0.0, 1.0);
        if f <= 0.002 {
            return;
        }
        if warn {
            self.br.SetColor(&color(1.0, 0.55, 0.45, a * self.alpha));
        } else {
            self.col(a);
        }
        if f >= 0.999 {
            self.rt.DrawEllipse(&D2D1_ELLIPSE { point: c, radiusX: r, radiusY: r }, self.br, w, None);
            return;
        }
        let Ok(fac) = self.rt.GetFactory() else { return };
        let Ok(geo) = fac.CreatePathGeometry() else { return };
        let Ok(sink) = geo.Open() else { return };
        let ang = f * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        sink.BeginFigure(D2D_POINT_2F { x: c.x, y: c.y - r }, D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F { x: c.x + r * ang.cos(), y: c.y + r * ang.sin() },
            size: D2D_SIZE_F { width: r, height: r },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: if f > 0.5 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        if sink.Close().is_err() {
            return;
        }
        // Flat caps: square arc ends, no rounded corners.
        let cap = D2D1_STROKE_STYLE_PROPERTIES { startCap: D2D1_CAP_STYLE_FLAT, endCap: D2D1_CAP_STYLE_FLAT, ..Default::default() };
        let style = fac.CreateStrokeStyle(&cap, None).ok();
        self.rt.DrawGeometry(&geo, self.br, w, style.as_ref());
    }
    unsafe fn circle(&self, x: f32, y: f32, r: f32, a: f32) {
        self.col(a);
        let e = D2D1_ELLIPSE { point: D2D_POINT_2F { x: self.ox + x, y: self.oy + y }, radiusX: r, radiusY: r };
        self.rt.FillEllipse(&e, self.br);
    }
}

pub struct Island {
    hwnd: HWND,
    cfg: IslandConfig,
    side: Side,
    rgb: (f32, f32, f32),
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    wic: Option<IWICImagingFactory>,
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    has_bmp: bool,
    rt: Option<ID2D1DCRenderTarget>,
    /// Wanted and current surface size in pixels.
    px: (i32, i32),
    surf: (i32, i32),
    fonts: Option<Fonts>,
    /// Blurred, saturated, downscaled monitor capture behind the open panel: (w, h, BGRA).
    snap: Option<(i32, i32, Vec<u8>)>,
    /// Mean luminance (0..1) of the backdrop behind the window, from the last capture.
    snap_lum: f32,
    snap_bmp: Option<ID2D1Bitmap>,
    noise_bmp: Option<ID2D1Bitmap>,
    /// Backdrop + tint + grain baked at window size; per-frame fills are then a 1:1 copy.
    glass: Option<([u32; 9], ID2D1Bitmap)>,
    /// Fractional wheel notches not yet applied, and an eased Sound slider: (target id, target, current).
    wheel: Option<(usize, f32, f32)>,
    wheel_hw: Instant,
    /// Cover art per session id with the generation it was decoded from; dropped with the render target.
    arts: std::collections::HashMap<String, (u32, Option<ID2D1Bitmap>)>,
    /// Shell icons by path; dropped whenever the render target is recreated.
    icons: std::collections::HashMap<String, Option<ID2D1Bitmap>>,
    tab: Tab,
    /// Playing sessions that get a full player (1..=MAX_PLAYERS), and paused ones shown as tiles (0..=MAX_TILES).
    players: usize,
    /// Top-dock width of one player: half the old box, grown only when the track text needs it.
    pw: f32,
    tiles: usize,
    /// Player being scrubbed.
    drag_p: usize,
    /// Per session id: eased (x, y, alpha) of its card, for layout animation.
    anim: std::collections::HashMap<String, (f32, f32, f32)>,
    anim_t: Instant,
    animating: bool,
    /// Spotify cover tint: which cover (session id, art gen) was analysed, its colour, and the eased colour in use.
    cov_key: (String, u32),
    /// Palette as 5 rgb triples: glass base, three glows, accent.
    cov_tgt: Option<[f32; 15]>,
    cov_cur: Option<[f32; 15]>,
    /// The cover shrunk to 48x48 and blurred (BGRA), and its bitmap on the current render target.
    cov_px: Option<Vec<u8>>,
    cov_bmp: Option<ID2D1Bitmap>,
    /// Row hover lift / press squash progress (0..1) for row `row_i`.
    row_t: f32,
    press_t: f32,
    /// Tab-switch progress (0..1), slide direction, and the smoothly-chasing scroll offset (rows).
    tab_a: f32,
    tab_dir: f32,
    scroll_f: f32,
    row_i: usize,
    row_anim: bool,
    s: f32,
    org: (i32, i32),
    mon: RECT,
    mode: Mode,
    v: f32,
    vel: f32,
    shown: bool,
    off_since: Option<Instant>,
    /// Copy "nudge": keep the peek pill out until this instant, then retract.
    nudge_until: Option<Instant>,
    /// When the window was hidden; the surface is freed 30 s later (kept meanwhile so reopening is instant).
    hid_at: Option<Instant>,
    peek_since: Instant,
    hover: Hover,
    drag: Option<f64>,
    /// (app id, duration) the seek bar belongs to, fixed at mouse-down.
    seek: Option<(String, f64)>,
    /// Row pressed at (screen x, y, index); becomes a drag-out once the mouse moves far enough.
    press: Option<(i32, i32, usize)>,
    scroll: usize,
    /// Clipboard filter: 0 all, 1 text, 2 files, 3 images, 4 pinned.
    filter: u8,
    flash: Option<(usize, Instant)>,
    /// Wheel HUD on the media tab: (is brightness, level 0..1, when).
    hud: Option<(bool, f32, Instant)>,
    store: clip::Store,
    /// System glance: previous (idle, total) CPU times, last sample, and when it was taken.
    sys_prev: (u64, u64),
    sys: Option<Sys>,
    sys_t: Instant,
    /// Sound tab: playing apps, master (level, muted), brightness (None: no DDC/CI), last refresh,
    /// slider being dragged (channel, or usize::MAX for brightness).
    mix: Vec<mixer::Chan>,
    master: (f32, bool),
    /// Per-monitor brightness, left to right (None: monitor supports neither DDC/CI nor WMI).
    bri: Vec<Option<f32>>,
    mix_t: Instant,
    slide: Option<usize>,
    /// "+" app picker: open?, first visible row, cached list (None until scanned), true = mixer pins,
    /// search query, and indexes of `apps` matching it.
    picker: bool,
    pick_scroll: usize,
    apps: Option<Vec<(String, String)>>,
    pick_mix: bool,
    pick_q: String,
    pick_hits: Vec<usize>,
    /// Note being edited on the Notes tab.
    edit: Option<usize>,
    /// Quick-launch and shelf paths (own file, so a settings save never clobbers them).
    items: Items,
    own_seq: u32,
    worker: Option<media::Worker>,
    last_tick: Instant,
    last_draw: Instant,
    dirty: bool,
    timer_ms: u32,
}

impl Island {
    pub fn create(cfg: &IslandConfig) -> Result<Island> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let hwnd = create_window()?;
            let _ = AddClipboardFormatListener(hwnd);
            DragAcceptFiles(hwnd, true); // Shelf / Launch drop-in
            if let Err(e) = windows::Win32::System::Ole::OleInitialize(None) {
                crate::warn!("island: OleInitialize failed ({e}); drag-out may not work");
            } // needed by DoDragDrop
            let worker = media::Worker::start(hwnd, WM_MEDIA);
            let now = Instant::now();
            let mut i = Island {
                hwnd,
                side: parse_side(&cfg.side),
                rgb: parse_rgb(&cfg.bg),
                cfg: cfg.clone(),
                d2d,
                dw,
                wic: None,
                dc: CreateCompatibleDC(None),
                bmp: HBITMAP::default(),
                old: HGDIOBJ::default(),
                has_bmp: false,
                rt: None,
                px: (1, 1),
                surf: (0, 0),
                fonts: None,
                snap: None,
                snap_lum: 0.2,
                snap_bmp: None,
                noise_bmp: None,
                glass: None,
                wheel: None,
                wheel_hw: Instant::now(),
                arts: Default::default(),
                icons: Default::default(),
                tab: Tab::Clip,
                players: 1,
                pw: TOP_MIN_W,
                tiles: 0,
                drag_p: 0,
                anim: Default::default(),
                anim_t: now,
                animating: false,
                cov_key: Default::default(),
                cov_tgt: None,
                cov_px: None,
                cov_bmp: None,
                cov_cur: None,
                row_t: 0.0,
                press_t: 0.0,
                tab_a: 1.0,
                tab_dir: 1.0,
                scroll_f: 0.0,
                row_i: 0,
                row_anim: false,
                s: 1.0,
                org: (0, 0),
                mon: RECT::default(),
                mode: Mode::Hidden,
                v: 0.0,
                vel: 0.0,
                shown: false,
                off_since: None,
                nudge_until: None,
                hid_at: None,
                peek_since: now,
                hover: Hover::None,
                drag: None,
                seek: None,
                press: None,
                scroll: 0,
                filter: 0,
                flash: None,
                hud: None,
                store: clip::Store::load(cfg.history_days),
                sys_prev: (0, 0),
                sys: None,
                sys_t: now.checked_sub(Duration::from_secs(5)).unwrap_or(now),
                mix: Vec::new(),
                master: (0.0, false),
                bri: Vec::new(),
                mix_t: now.checked_sub(Duration::from_secs(5)).unwrap_or(now),
                slide: None,
                picker: false,
                pick_scroll: 0,
                apps: None,
                pick_mix: false,
                pick_q: String::new(),
                pick_hits: Vec::new(),
                edit: None,
                items: Items::load(),
                own_seq: 0,
                worker: Some(worker),
                last_tick: now,
                last_draw: now,
                dirty: false,
                timer_ms: 0,
            };
            i.timer(60);
            Ok(i)
        }
    }

    /// For `--island-snapshot`: draws the open island on `tab` into its own bitmap and returns premultiplied BGRA.
    /// Mixer, notes and launch items are sample data so nothing personal (clipboard history, saved notes) is rendered.
    pub fn snapshot(&mut self, tab: &str) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            self.cfg.blur = 0;
            self.side = Side::Top; // the notch-from-the-top layout
            self.tab = match tab {
                "media" => Tab::Media,
                "sound" => Tab::Sound,
                "launch" => Tab::Launch,
                "system" => Tab::System,
                "notes" | "shelf" => Tab::Shelf,
                _ => Tab::Clip,
            };
            let chan = |n: &str, v: f32, path: String| mixer::Chan { key: n.into(), name: n.into(), path, vol: v, mute: false };
            let appdata = std::env::var("APPDATA").unwrap_or_default();
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            self.mix = vec![
                chan("Spotify", 0.72, format!(r"{appdata}\Spotify\Spotify.exe")),
                chan("Discord", 0.45, format!(r"{local}\Discord\Update.exe")),
            ];
            self.master = (0.6, false);
            self.bri = vec![Some(0.72)];
            self.items.notes = vec!["Reply to Sam about the build".into(), "Ship 0.5.4, tidy settings".into()];
            self.items.launch = [r"C:\Windows\System32\notepad.exe", r"C:\Windows\System32\calc.exe", r"C:\Windows\System32\mspaint.exe", r"C:\Windows\explorer.exe"]
                .iter()
                .map(|p| p.to_string())
                .collect();
            if self.tab == Tab::Media {
                self.seed_media();
            }
            self.mode = Mode::Open;
            self.show();
            self.v = 1.0;
            self.vel = 0.0;
            self.tab_a = 1.0;
            self.sample_sys();
            // Paint until card fade-ins settle; the hidden layered window may refuse the update, the bitmap is drawn either way.
            for _ in 0..40 {
                let _ = self.paint();
                if !self.animating {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            if self.rt.is_none() || !self.has_bmp {
                return None;
            }
            let (w, h) = (self.px.0 as u32, self.px.1 as u32);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u8; (w * h * 4) as usize];
            let _ = GdiFlush();
            // GetDIBits needs the bitmap out of its DC; put it back afterwards.
            SelectObject(self.dc, self.old);
            let rows = GetDIBits(self.dc, self.bmp, 0, h, Some(px.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
            SelectObject(self.dc, self.bmp);
            self.hide();
            if rows == 0 {
                return None;
            }
            Some((w, h, px))
        }
    }

    /// For `--island-snapshot media`: one sample Spotify session with a generated cover, so the real Media tab
    /// (cover, colour tint, controls) renders without anything having to be playing.
    fn seed_media(&mut self) {
        if let Some(wk) = self.worker.as_ref() {
            wk.active.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(600)); // let the worker's first poll finish before overriding it
            *wk.state.lock().unwrap_or_else(|e| e.into_inner()) = vec![media::Media {
                id: "Spotify.exe".into(),
                title: "Neon Drift".into(),
                artist: "Lumen Sample".into(),
                playing: true,
                pos: 71.0,
                dur: 243.0,
                at: Instant::now(),
                art: Some(Arc::new(sample_cover())),
                art_gen: 1,
            }];
        }
        self.step_cover(f32::INFINITY);
    }

    pub fn set_config(&mut self, cfg: &IslandConfig) {
        if cfg.font != self.cfg.font || cfg.font_size != self.cfg.font_size {
            self.fonts = None;
        }
        let reshoot = cfg.blur != self.cfg.blur || cfg.saturation != self.cfg.saturation;
        self.cfg = cfg.clone();
        if reshoot {
            self.snap = None;
            self.snap_bmp = None;
            self.glass = None;
        }
        self.side = parse_side(&cfg.side);
        self.rgb = parse_rgb(&cfg.bg);
        self.store.set_days(cfg.history_days);
        if !self.tabs().iter().any(|t| t.2 == self.tab) {
            self.tab = Tab::Clip;
        }
        if !cfg.clip_tools || (self.filter == 3 && !cfg.clip_images) {
            self.filter = 0;
        }
        if self.shown {
            let was = (self.org, self.px);
            unsafe { self.place() };
            self.glass = None;
            // Re-shoot only if place() moved/resized the window (snap_lum is measured there); a re-shoot while
            // open would also grab our own panel, and per-slider-tick full-monitor blurs are costly.
            if self.mode == Mode::Open && was != (self.org, self.px) {
                self.snap_bmp = None;
                unsafe { self.capture() };
            }
            self.touch();
        }
    }

    // ---- geometry -------------------------------------------------------------------------

    /// Content box (w, h): wide on the top edge, tall on the sides. Same for both tabs.
    fn cb(&self) -> (f32, f32) {
        let (np, t) = if self.tab == Tab::Media { (self.players.max(1), self.tiles as f32) } else { (1, 0.0) };
        if self.tab == Tab::Sound {
            // Grow sideways so every app column (+ the "+" column) and every brightness pill fits.
            let n = self.bri.len().max(1) as f32;
            let need = self.col_x(self.mix.len() + 1) + COL_W + 14.0 + n * BRI_W + (n - 1.0) * BRI_GAP + PAD;
            let base = if self.side == Side::Top { (TOP_MIN_W, TOP_BOX.1) } else { SIDE_BOX };
            return (base.0.max(need).min(1600.0), base.1);
        }
        if self.side == Side::Top {
            let w = if self.tab == Tab::Media { self.player_w() * np as f32 + t * (TILE_W + TILE_GAP) } else { TOP_MIN_W };
            (w, TOP_BOX.1)
        } else if t > 0.0 {
            (SIDE_BOX.0, SIDE_BOX.1.max(self.tiles_y() + t * (TILE_H + TILE_GAP) - TILE_GAP + 20.0))
        } else if np > 1 {
            (SIDE_BOX.0, SIDE_BOX.1.max(MULTI_Y + np as f32 * MULTI_H + 20.0))
        } else {
            SIDE_BOX
        }
    }

    /// Sound tab: (track height, app columns that fit incl. master, x of the first brightness pill).
    /// One column after the apps is kept for the "+" pin button.
    fn sound_lay(&self) -> (f32, usize, f32) {
        let (bw, bh) = self.cb();
        let th = (bh - SND_Y - MUTE_D - 40.0).clamp(90.0, 220.0);
        let n = self.bri.len().max(1) as f32;
        let bx = bw - PAD - n * BRI_W - (n - 1.0) * BRI_GAP;
        let fit = ((bx - 14.0 - self.col_x(1) + COL_GAP) / (COL_W + COL_GAP)).max(1.0) as usize;
        (th, 1 + (fit - 1).min(self.mix.len()), bx)
    }

    /// X of brightness pill i.
    fn pill_x(bx: f32, i: usize) -> f32 {
        bx + i as f32 * (BRI_W + BRI_GAP)
    }

    /// Left x of mixer column n (0 = master, then a divider, then apps).
    fn col_x(&self, n: usize) -> f32 {
        if n == 0 { PAD } else { PAD + COL_W + 16.0 + (n - 1) as f32 * (COL_W + COL_GAP) }
    }

    fn sample_mix(&mut self) {
        self.mix_t = Instant::now();
        sample_mix_async(self.hwnd, self.items.mix_pins.clone());
    }

    /// Level 0..1 from a y inside a track of height th starting at SND_Y.
    fn level(y: f32, th: f32) -> f32 {
        (1.0 - (y - SND_Y) / th).clamp(0.0, 1.0)
    }

    /// Drag/click on slider n (or usize::MAX - i for monitor i's brightness) at y.
    fn slide_to(&mut self, n: usize, y: f32) {
        let (th, ..) = self.sound_lay();
        self.wheel = None;
        if n > usize::MAX - 64 {
            self.set_level(n, Self::level(y, th + MUTE_D + 6.0), true);
            return;
        }
        self.set_level(n, Self::level(y, th), true);
    }

    /// Current level of mixer id `n` (0 master, i+1 app i, usize::MAX-i brightness i).
    fn level_of(&self, n: usize) -> Option<f32> {
        if n > usize::MAX - 64 {
            return self.bri.get(usize::MAX - n).copied().flatten();
        }
        if n == 0 { Some(self.master.0) } else { self.mix.get(n - 1).map(|c| c.vol) }
    }

    /// Sets a level on screen; `hw` also pushes it to the device (callers throttle easing frames).
    fn set_level(&mut self, n: usize, v: f32, hw: bool) {
        if n > usize::MAX - 64 {
            let i = usize::MAX - n;
            if let Some(b) = self.bri.get_mut(i) {
                *b = Some(v);
                if hw { push_brightness(i, v); }
            }
            return;
        }
        if n == 0 {
            self.master = (v, false);
            if hw { mixer::set_master(Some(v), Some(false)); }
        } else if let Some(ch) = self.mix.get_mut(n - 1) {
            ch.vol = v;
            ch.mute = false;
            if hw { mixer::set_app(&ch.key, Some(v), Some(false)); }
        }
    }

    fn toggle_mute(&mut self, n: usize) {
        self.wheel = None;
        if n == 0 {
            self.master.1 = !self.master.1;
            mixer::set_master(None, Some(self.master.1));
        } else if let Some(ch) = self.mix.get_mut(n - 1) {
            ch.mute = !ch.mute;
            mixer::set_app(&ch.key, None, Some(ch.mute));
        }
    }

    /// Width of one player on the top dock: the full box alone, narrower when they share.
    fn player_w(&self) -> f32 {
        self.pw
    }

    /// Y where the tile column starts on a side dock.
    fn tiles_y(&self) -> f32 {
        if self.players.max(1) == 1 { SIDE_TILES_Y } else { MULTI_Y + self.players as f32 * MULTI_H + 8.0 }
    }

    /// Card rect (x, y, w, h) of media tile `i` in content coordinates.
    fn tile(&self, i: usize) -> (f32, f32, f32, f32) {
        let i = i as f32;
        if self.side == Side::Top {
            (self.player_w() * self.players.max(1) as f32 - PAD + TILE_GAP + i * (TILE_W + TILE_GAP), 58.0, TILE_W, 178.0)
        } else {
            (PAD, self.tiles_y() + i * (TILE_H + TILE_GAP), SIDE_BOX.0 - 2.0 * PAD, TILE_H)
        }
    }

    /// Sessions with the playing ones first (stable), so paused ones fall to tiles.
    fn ordered(&self) -> Vec<media::Media> {
        let Some(wk) = self.worker.as_ref() else { return Vec::new() };
        let mut v = wk.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        v.sort_by_key(|m| !m.playing);
        v
    }

    /// (players, tiles) for a session list already ordered playing-first.
    fn counts(v: &[media::Media]) -> (usize, usize) {
        let playing = v.iter().filter(|m| m.playing).count();
        let np = playing.clamp(1, MAX_PLAYERS).min(v.len());
        (np, (v.len() - np).min(MAX_TILES))
    }

    /// Width one top-dock player needs: art + text, never below the minimum, capped per layout.
    fn fit(v: &[media::Media], np: usize) -> f32 {
        let est = |s: &str, per: f32| s.chars().map(|c| if c.is_ascii() { per } else { per * 1.9 }).sum::<f32>();
        let need = v.iter().take(np).map(|m| est(&m.title, 7.4).max(est(&m.artist, 6.4)).max(150.0)).fold(0.0, f32::max);
        let cap = if np == 1 { TOP_MAX_W } else { MULTI_W };
        (218.0 + need + PAD + 8.0).clamp(TOP_MIN_W, cap)
    }

    /// Resizes the panel when the number of players or tiles it should show changes.
    unsafe fn sync_tiles(&mut self) {
        let v = self.ordered();
        let (p, t) = Self::counts(&v);
        let p = p.max(1);
        let pw = Self::fit(&v, p);
        if t != self.tiles || p != self.players || (pw - self.pw).abs() > 0.5 {
            self.tiles = t;
            self.players = p;
            self.pw = pw;
            if self.tab == Tab::Media && self.shown {
                self.place();
            }
        }
    }

    fn win_design(&self) -> (f32, f32) {
        let (w, h) = self.cb();
        if self.side == Side::Top { (w + 2.0 * F, h) } else { (w, h + 2.0 * F) }
    }

    /// Panel-local (u along the depth axis, t along the edge from its centre) to window design coords.
    fn m(&self, u: f32, t: f32) -> (f32, f32) {
        let (w, h) = self.cb();
        match self.side {
            Side::Left => (u, F + h / 2.0 + t),
            Side::Right => (w - u, F + h / 2.0 + t),
            Side::Top => (F + w / 2.0 + t, u),
        }
    }

    /// Where the content box starts inside the window.
    fn co(&self) -> (f32, f32) {
        if self.side == Side::Top { (F, 0.0) } else { (0.0, F) }
    }

    fn maxes(&self) -> (f32, f32) {
        let (w, h) = self.cb();
        if self.side == Side::Top { (h, w / 2.0) } else { (w, h / 2.0) }
    }

    /// Clipboard rows that fit under the tab bar and the filter strip.
    fn rows(&self) -> usize {
        (((self.cb().1 - LIST_Y - 16.0 + ROW_GAP) / ROW_PITCH) as usize).max(1)
    }

    /// Enabled tabs with their x and width; Clip and Media are always there.
    fn tabs(&self) -> Vec<(f32, f32, Tab)> {
        let c = &self.cfg;
        let mut x = PAD;
        let mut v = Vec::new();
        for t in [Tab::Clip, Tab::Media, Tab::Sound, Tab::Launch, Tab::System, Tab::Shelf] {
            let on = match t {
                Tab::Sound => c.media_controls,
                Tab::Launch => c.tab_launch,
                Tab::System => c.tab_system,
                Tab::Shelf => c.tab_shelf,
                _ => true,
            };
            if on {
                let w = if c.tab_icons { 38.0 } else { t.width() };
                v.push((x, w, t));
                x += w + TAB_GAP;
            }
        }
        // Icon tabs are narrow: centre the row instead of hugging the left edge.
        if c.tab_icons {
            let shift = ((self.cb().0 - (x - TAB_GAP + PAD)) / 2.0).max(0.0);
            for t in &mut v {
                t.0 += shift;
            }
        }
        v
    }

    /// Store indexes shown by the current clipboard filter, newest first.
    fn view(&self) -> Vec<usize> {
        let f = if self.cfg.clip_tools { self.filter } else { 0 };
        self.store
            .items
            .iter()
            .enumerate()
            .filter(|(_, e)| match f {
                1 => !e.files,
                2 => e.files && !e.img,
                3 => e.img,
                4 => e.pin,
                _ => true,
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Filter chips on the clipboard strip: (filter id, label, width).
    fn chips(&self) -> Vec<(u8, &'static str, f32)> {
        let mut v = vec![(0, "All", 40.0), (1, "Text", 46.0), (2, "Files", 48.0)];
        if self.cfg.clip_images {
            v.push((3, "Images", 60.0));
        }
        v.push((4, "Pinned", 60.0));
        v
    }

    /// Layout of player `i`; players share the box equally, tiles get the extra space cb() adds.
    fn lay_n(&self, i: usize) -> Lay {
        let i = i as f32;
        if self.side == Side::Top {
            let pw = self.player_w();
            let x0 = i * pw;
            let tx = x0 + 218.0;
            let tw = pw - PAD - 218.0;
            let mid = tx + tw / 2.0;
            Lay { art: (x0 + PAD, 62.0, 170.0), tx, tw, ty: 66.0, sy: 150.0, cy: 206.0, cxs: [mid - 64.0, mid, mid + 64.0] }
        } else if self.players.max(1) == 1 {
            let w = SIDE_BOX.0;
            Lay { art: ((w - 200.0) / 2.0, 60.0, 200.0), tx: PAD, tw: w - 2.0 * PAD, ty: 276.0, sy: 340.0, cy: 408.0, cxs: [w / 2.0 - 60.0, w / 2.0, w / 2.0 + 60.0] }
        } else {
            let w = SIDE_BOX.0;
            let y0 = MULTI_Y + i * MULTI_H;
            Lay { art: (PAD, y0 + 6.0, 84.0), tx: PAD + 100.0, tw: w - PAD - 124.0, ty: y0 + 4.0, sy: y0 + 72.0, cy: y0 + 140.0, cxs: [w / 2.0 - 60.0, w / 2.0, w / 2.0 + 60.0] }
        }
    }

    /// (id, duration) of player `i`.
    fn nth(&self, i: usize) -> Option<(String, f64)> {
        self.ordered().get(i).map(|x| (x.id.clone(), x.dur))
    }

    /// Id of tile `i`, which follows the players.
    fn sec_id(&self, i: usize) -> Option<String> {
        self.ordered().get(self.players.max(1) + i).map(|x| x.id.clone())
    }

    fn media_len(&self) -> usize {
        self.worker.as_ref().and_then(|w| Some(w.state.lock().unwrap_or_else(|e| e.into_inner()).len())).unwrap_or(0)
    }

    /// (depth, half-length, openness 0..1) for the current spring value.
    fn shape(&self) -> (f32, f32, f32) {
        let (emax, hmax) = self.maxes();
        let v = self.v;
        if v <= 0.0 {
            return (0.0, 0.0, 0.0);
        }
        if v < PEEK {
            let q = v / PEEK;
            return (PEEK_EXT * q, PEEK_HH * q, 0.0);
        }
        let p = (v - PEEK) / (1.0 - PEEK);
        let k = p.clamp(0.0, 1.0);
        let lerp = |a: f32, b: f32| a + (b - a) * p;
        (lerp(PEEK_EXT, emax).clamp(0.0, emax), lerp(PEEK_HH, hmax).clamp(0.0, hmax), k)
    }

    fn bounds(&self, ext: f32, hh: f32) -> (f32, f32, f32, f32) {
        let a = self.m(0.0, -hh);
        let b = self.m(ext, hh);
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }

    fn screen_rect(&self, ext: f32, hh: f32, slop: i32) -> RECT {
        let (l, t, r, b) = self.bounds(ext, hh);
        let s = self.s;
        RECT {
            left: self.org.0 + (l * s) as i32 - slop,
            top: self.org.1 + (t * s) as i32 - slop,
            right: self.org.0 + (r * s).ceil() as i32 + slop,
            bottom: self.org.1 + (b * s).ceil() as i32 + slop,
        }
    }

    unsafe fn place(&mut self) {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            self.mon = mi.rcMonitor;
        }
        let (mw, mh) = ((self.mon.right - self.mon.left) as f32, (self.mon.bottom - self.mon.top) as f32);
        let (ww, wh) = self.win_design();
        self.s = (GetDpiForSystem() as f32 / 96.0 * self.cfg.scale).min(mw / ww).min(mh / wh).max(0.3);
        let (pw, ph) = ((ww * self.s).ceil() as i32, (wh * self.s).ceil() as i32);
        self.px = (pw, ph);
        let (cx, cy) = (self.mon.left + (mw as i32 - pw) / 2, self.mon.top + (mh as i32 - ph) / 2);
        self.org = match self.side {
            Side::Left => (self.mon.left, cy),
            Side::Right => (self.mon.right - pw, cy),
            Side::Top => (cx, self.mon.top),
        };
    }

    // ---- state ----------------------------------------------------------------------------

    fn instant(&self) -> bool {
        self.cfg.animation == "none" || self.cfg.animation_ms == 0
    }

    fn target(&self) -> f32 {
        match self.mode {
            Mode::Hidden => 0.0,
            Mode::Peek => PEEK,
            Mode::Open => 1.0,
        }
    }

    fn touch(&mut self) {
        self.dirty = true;
        // Paint right away (not on the next timer tick) so input feels instant; the tick catches the rest.
        if self.shown && self.last_draw.elapsed() >= Duration::from_millis(8) {
            unsafe {
                if self.paint().is_ok() {
                    self.dirty = false;
                    self.last_draw = Instant::now();
                }
            }
        }
        self.timer(16);
    }

    fn timer(&mut self, ms: u32) {
        FAST.store(ms <= 16, Ordering::Relaxed);
        if self.timer_ms != ms {
            self.timer_ms = ms;
            unsafe { SetTimer(self.hwnd, T_TICK, ms, None) };
        }
    }

    fn set_mode(&mut self, m: Mode) {
        if self.mode == m {
            return;
        }
        let was = self.mode;
        self.mode = m;
        self.off_since = None;
        self.nudge_until = None;
        self.press = None;
        self.seek = None;
        unsafe {
            if was == Mode::Hidden && !self.shown {
                self.show();
            }
            if let Some(wk) = &self.worker {
                wk.active.store(m != Mode::Hidden, Ordering::Relaxed);
                if m != Mode::Hidden {
                    wk.send(media::Cmd::Refresh);
                }
            }
            if m != Mode::Open {
                self.hover = Hover::None;
                if self.drag.take().is_some() {
                    let _ = ReleaseCapture();
                }
                self.store.save();
            }
        }
        if m != Mode::Hidden {
            self.step_cover(f32::INFINITY); // first paint already has the right colour
        }
        if m == Mode::Open {
            unsafe { self.capture() };
            self.scroll = 0;
            self.scroll_f = 0.0;
            self.tab_a = 1.0;
        }
        if self.instant() {
            self.v = self.target();
            self.vel = 0.0;
        }
        self.touch();
    }

    unsafe fn show(&mut self) {
        let v = self.ordered();
        let (p, t) = Self::counts(&v);
        self.players = p.max(1);
        self.tiles = t;
        self.pw = Self::fit(&v, self.players);
        self.place();
        let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        self.shown = true;
    }

    unsafe fn hide(&mut self) {
        let _ = ShowWindow(self.hwnd, SW_HIDE);
        self.shown = false;
        self.animating = false;
        self.hid_at = Some(Instant::now());
    }

    unsafe fn release_surface(&mut self) {
        self.rt = None;
        self.snap_bmp = None;
        self.glass = None;
        self.noise_bmp = None;
        self.cov_bmp = None;
        self.arts.clear();
        self.fonts = None;
        if self.has_bmp {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            self.has_bmp = false;
        }
        self.surf = (0, 0);
    }

    /// Advances the spring. Returns true while something moved.
    fn step(&mut self, dt: f32) -> bool {
        let t = self.target();
        if self.instant() {
            let moved = self.v != t;
            self.v = t;
            self.vel = 0.0;
            return moved;
        }
        if self.v == t && self.vel == 0.0 {
            return false;
        }
        // Slower, springier morph: the pill stretches out and settles with a little bounce.
        let omega = 6.0 / (self.cfg.animation_ms.max(1) as f32 * 2.6 / 1000.0);
        let zeta = if self.cfg.animation == "liquid" { 0.42 } else { 0.68 };
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(0.008);
            left -= h;
            let a = -omega * omega * (self.v - t) - 2.0 * zeta * omega * self.vel;
            self.vel += a * h;
            self.v += self.vel * h;
        }
        if self.v < 0.0 {
            self.v = 0.0;
            self.vel = 0.0;
        }
        self.v = self.v.min(1.25);
        if (self.v - t).abs() < 0.002 && self.vel.abs() < 0.01 {
            self.v = t;
            self.vel = 0.0;
        }
        true
    }

    /// Spotify cover tint: reads the front player's cover once per track, then eases the glass colour to it.
    /// Returns true while the colour is still moving. `dt` of infinity snaps.
    fn step_cover(&mut self, dt: f32) -> bool {
        let front = if self.cfg.cover_colors { self.ordered().into_iter().next().filter(|m| m.id.to_ascii_lowercase().contains("spotify")) } else { None };
        let key = front.as_ref().map(|m| (m.id.clone(), m.art_gen)).unwrap_or_default();
        if key != self.cov_key {
            if self.wic.is_none() {
                self.wic = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
            }
            let art = front.and_then(|m| m.art);
            self.cov_tgt = match (&self.wic, &art) {
                (Some(wic), Some(b)) => unsafe { cover_palette(wic, b) },
                _ => None,
            };
            self.cov_px = match (&self.wic, &art) {
                (Some(wic), Some(b)) => unsafe { cover_blur(wic, b) },
                _ => None,
            };
            self.cov_bmp = None;
            self.cov_key = key;
        }
        let (Some(t), Some(c)) = (self.cov_tgt, self.cov_cur) else {
            // Switching on or off is immediate; only cover-to-cover changes ease.
            self.cov_cur = self.cov_tgt;
            return false;
        };
        let k = 1.0 - (-dt * 9.0).exp();
        let mut n = c;
        for i in 0..15 {
            n[i] += (t[i] - c[i]) * k;
        }
        if (0..15).all(|i| (t[i] - n[i]).abs() < 0.004) {
            n = t;
        }
        self.cov_cur = Some(n);
        n != t
    }

    /// Eases the hovered row's lift and the pressed row's squash; true while still moving.
    fn step_rows(&mut self, dt: f32) -> bool {
        let k = dt / (self.cfg.animation_ms.max(1) as f32 / 1000.0);
        let instant = self.instant();
        let mut tgt = 0.0;
        if let Hover::Row(i) = self.hover {
            if i != self.row_i {
                self.row_i = i;
                self.row_t = 0.0;
            }
            tgt = 1.0;
        }
        let ptgt = if self.press.is_some() { 1.0 } else { 0.0 };
        let ease = |v: f32, t: f32| if instant { t } else if v < t { (v + k).min(t) } else { (v - k).max(t) };
        let (a, b) = (ease(self.row_t, tgt), ease(self.press_t, ptgt));
        let mut moving = a != self.row_t || b != self.press_t;
        self.row_t = a;
        self.press_t = b;
        // Tab slide/fade-in.
        if self.tab_a < 1.0 {
            self.tab_a = if instant { 1.0 } else { (self.tab_a + dt / 0.26).min(1.0) };
            moving = true;
        }
        // Smooth scroll: chase the target row offset exponentially.
        let tgt = self.scroll as f32;
        if self.scroll_f != tgt {
            if instant {
                self.scroll_f = tgt;
            } else {
                self.scroll_f += (tgt - self.scroll_f) * (1.0 - (-dt * 16.0).exp());
                if (tgt - self.scroll_f).abs() < 0.01 {
                    self.scroll_f = tgt;
                }
            }
            moving = true;
        }
        moving
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        unsafe { self.pointer(now) };
        let moving = self.step(dt) | self.step_cover(dt);
        self.row_anim = self.step_rows(dt);
        if let Some((id, t, c)) = self.wheel {
            let c = if (t - c).abs() < 0.002 { t } else { c + (t - c) * (1.0 - (-dt * 18.0).exp()) };
            self.wheel = (c != t).then_some((id, t, c));
            // Device calls are slow (session enumeration); push every ~50 ms and on the last frame.
            let hw = c == t || self.wheel_hw.elapsed() >= Duration::from_millis(50);
            if hw { self.wheel_hw = now; }
            self.set_level(id, c, hw);
            // Only the media-tab wheel drives the HUD; Sound-tab faders must not hijack it.
            if let Some((bright, _, _)) = self.hud {
                if if bright { id == usize::MAX - mixer::monitor_of(self.hwnd) } else { id == 0 } {
                    self.hud = Some((bright, c, now));
                }
            }
            self.dirty = true;
        }
        let mut flashing = false;
        if let Some((_, t)) = self.flash {
            if t.elapsed() < Duration::from_millis(900) {
                flashing = true;
            } else {
                self.flash = None;
                self.dirty = true;
            }
        }
        if let Some((_, _, t)) = self.hud {
            if t.elapsed() < Duration::from_millis(1400) {
                flashing = true;
            } else {
                self.hud = None;
                self.dirty = true;
            }
        }
        if self.mode == Mode::Open && now.duration_since(self.last_draw) >= Duration::from_millis(1000) {
            if let Some(wk) = &self.worker {
                if wk.state.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|x| x.playing) {
                    self.dirty = true;
                }
            }
        }
        if self.mode == Mode::Open && self.tab == Tab::System && now.duration_since(self.sys_t) >= Duration::from_secs(1) {
            // Repaint only when a shown figure changes.
            let key = |s: Option<Sys>| s.map(|s| ((s.cpu * 100.0) as u32, (s.ram * 100.0) as u32, s.bat, s.uptime_s / 60, s.disk_free >> 26));
            let before = key(self.sys);
            unsafe { self.sample_sys() };
            if key(self.sys) != before {
                self.dirty = true;
            }
        }
        if self.mode == Mode::Open && self.tab == Tab::Sound && self.slide.is_none() && self.wheel.is_none() && now.duration_since(self.mix_t) >= Duration::from_secs(1) {
            let before = (self.mix.clone(), self.master);
            self.sample_mix();
            if (self.mix.clone(), self.master) != before {
                self.dirty = true;
            }
        }
        if self.mode == Mode::Hidden && !moving && self.v <= 0.0 && self.shown {
            unsafe { self.hide() };
            self.dirty = false;
        }
        if let Some(t) = self.hid_at {
            if self.shown {
                self.hid_at = None;
            } else if t.elapsed() >= Duration::from_secs(30) {
                self.hid_at = None;
                unsafe { self.release_surface() };
            }
        }
        if !self.shown {
            // Nothing to paint while hidden; a stale dirty flag would pin the 16 ms vsync loop on forever (show() repaints anyway).
            self.dirty = false;
        }
        if self.shown && (moving || self.dirty || flashing || self.animating || self.row_anim) {
            unsafe {
                if self.paint().is_err() {
                    let _ = self.paint();
                }
            }
            self.dirty = false;
            self.last_draw = now;
        }
        self.timer(if moving || self.dirty || flashing || self.animating || self.row_anim { 16 } else if self.mode == Mode::Hidden { 25 } else { 33 }); // hidden: 40 Hz edge poll is plenty, 100 Hz burned idle CPU
    }

    unsafe fn pointer(&mut self, now: Instant) {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
        if self.mon.right == self.mon.left {
            self.place();
        }
        let down = (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0;
        let (cx, cy) = ((self.mon.left + self.mon.right) / 2, (self.mon.top + self.mon.bottom) / 2);
        let (depth, along) = match self.side {
            Side::Left => (pt.x - self.mon.left, pt.y - cy),
            Side::Right => (self.mon.right - 1 - pt.x, pt.y - cy),
            Side::Top => (pt.y - self.mon.top, pt.x - cx),
        };
        let band = (100.0 * self.s) as i32;
        let at_edge = depth <= 1 && along.abs() <= band;
        // The notch shows up as soon as the cursor is in the screen quarter next to the docked edge.
        let span = if self.side == Side::Top { self.mon.bottom - self.mon.top } else { self.mon.right - self.mon.left };
        let near = depth <= span / 4 && along.abs() <= (300.0 * self.s) as i32;
        let (emax, hmax) = self.maxes();
        let inside = |r: RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
        match self.mode {
            Mode::Hidden => {
                if near && !down {
                    let busy = SHQueryUserNotificationState()
                        .map(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE)
                        .unwrap_or(false);
                    if !busy {
                        self.peek_since = now;
                        self.set_mode(Mode::Peek);
                    }
                }
            }
            Mode::Peek => {
                let on = inside(self.screen_rect(PEEK_EXT, PEEK_HH, 8));
                // Dragging a window (title bar) to the edge must not open it; file drags still can.
                let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
                let moving_win = GetGUIThreadInfo(0, &mut gti).is_ok() && gti.flags.0 & GUI_INMOVESIZE.0 != 0;
                if !moving_win && ((on && depth >= 2) || (at_edge && now.duration_since(self.peek_since) >= Duration::from_millis(30))) {
                    self.set_mode(Mode::Open);
                } else if !on && !at_edge && !near && self.nudge_until.is_some_and(|t| now < t) {
                    self.off_since = None;
                } else if !on && !at_edge && !near {
                    self.nudge_until = None;
                    let since = *self.off_since.get_or_insert(now);
                    if now.duration_since(since) >= Duration::from_millis(250) {
                        self.set_mode(Mode::Hidden);
                    }
                } else {
                    self.off_since = None;
                }
            }
            Mode::Open => {
                if inside(self.screen_rect(emax, hmax, 12)) || self.drag.is_some() || self.press.is_some() {
                    self.off_since = None;
                } else {
                    let since = *self.off_since.get_or_insert(now);
                    if now.duration_since(since) >= Duration::from_millis(200) {
                        self.set_mode(Mode::Hidden);
                    }
                }
            }
        }
    }

    // ---- drawing --------------------------------------------------------------------------

    unsafe fn ensure_surface(&mut self) -> Result<()> {
        if self.fonts.is_none() {
            self.fonts = Some(Fonts::new(&self.dw, &self.cfg.font, self.cfg.font_size)?);
        }
        if self.rt.is_some() && self.surf == self.px {
            return Ok(());
        }
        self.rt = None;
        self.snap_bmp = None;
        self.glass = None;
        self.noise_bmp = None;
        self.cov_bmp = None;
        self.arts.clear();
        if !self.has_bmp || self.surf != self.px {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: self.px.0,
                    biHeight: -self.px.1,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let prev = SelectObject(self.dc, bmp);
            if self.has_bmp {
                let _ = DeleteObject(prev);
            } else {
                self.old = prev;
            }
            self.bmp = bmp;
            self.has_bmp = true;
            self.surf = self.px;
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        self.rt = Some(self.d2d.CreateDCRenderTarget(&props)?);
        Ok(())
    }

    unsafe fn paint(&mut self) -> Result<()> {
        self.ensure_surface()?;
        let rt = self.rt.clone().unwrap();
        rt.SetDpi(96.0 * self.s, 96.0 * self.s);
        rt.BindDC(self.dc, &RECT { left: 0, top: 0, right: self.px.0, bottom: self.px.1 })?;
        rt.BeginDraw();
        rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        // Media card easing re-arms this each frame it draws; anything else must not keep the vsync loop alive.
        self.animating = false;
        let drawn = self.draw(&rt);
        let ended = rt.EndDraw(None, None);
        if drawn.is_err() || ended.is_err() {
            self.rt = None;
            self.snap_bmp = None;
        self.glass = None;
            self.noise_bmp = None;
            self.cov_bmp = None;
            self.arts.clear();
            self.icons.clear();
            return drawn.and(ended);
        }
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(
            self.hwnd,
            None,
            Some(&POINT { x: self.org.0, y: self.org.1 }),
            Some(&SIZE { cx: self.px.0, cy: self.px.1 }),
            self.dc,
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }

    unsafe fn geometry(&self, ext: f32, hh: f32, k: f32) -> Result<ID2D1PathGeometry> {
        let f = F.min(ext * 0.5);
        let r = (12.0 + (self.cfg.radius - 12.0) * k).min(hh).min((ext - f).max(0.0));
        let p = |u: f32, t: f32| {
            let (x, y) = self.m(u, t);
            D2D_POINT_2F { x, y }
        };
        let q = |cu: f32, ct: f32, u: f32, t: f32| D2D1_QUADRATIC_BEZIER_SEGMENT { point1: p(cu, ct), point2: p(u, t) };
        let geo = self.d2d.CreatePathGeometry()?;
        let sink = geo.Open()?;
        sink.BeginFigure(p(0.0, -hh - f), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddQuadraticBezier(&q(0.0, -hh, f, -hh));
        sink.AddLine(p(ext - r, -hh));
        sink.AddQuadraticBezier(&q(ext, -hh, ext, -hh + r));
        sink.AddLine(p(ext, hh - r));
        sink.AddQuadraticBezier(&q(ext, hh, ext - r, hh));
        sink.AddLine(p(f, hh));
        sink.AddQuadraticBezier(&q(0.0, hh, 0.0, hh + f));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        Ok(geo)
    }

    /// Tint colour and alpha for the glass, from the theme knobs.
    fn tint(&self) -> (f32, f32, f32, f32) {
        let a = self.cfg.opacity;
        if let Some(p) = self.cov_cur {
            return (p[0], p[1], p[2], a.max(0.6));
        }
        match self.cfg.tint.as_str() {
            "light" => (250.0 / 255.0, 251.0 / 255.0, 253.0 / 255.0, a),
            "accent" => {
                let (r, g, b) = hsl(self.cfg.accent_hue as f32, 0.6, 0.22);
                (r, g, b, a)
            }
            "clear" => (1.0, 1.0, 1.0, a * 0.6),
            _ => (self.rgb.0, self.rgb.1, self.rgb.2, a),
        }
    }

    /// The closed notch: a dark orb carrying the Beam L mark, scaled by q, at opacity a.
    unsafe fn orb(&self, rt: &ID2D1DCRenderTarget, br: &ID2D1SolidColorBrush, q: f32, a: f32) -> Result<()> {
        if q <= 0.02 || a <= 0.0 {
            return Ok(());
        }
        let (cx, cy) = self.m(21.0 * q, 0.0);
        let r = 15.0 * q;
        let el = |rad: f32| D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: rad, radiusY: rad };
        br.SetColor(&color(0.0, 0.0, 0.0, 0.06 * a));
        rt.FillEllipse(&el(r + 4.0 * q), br);
        br.SetColor(&color(0.0, 0.0, 0.0, 0.12 * a));
        rt.FillEllipse(&el(r + 2.0 * q), br);
        br.SetColor(&color(11.0 / 255.0, 12.0 / 255.0, 15.0 / 255.0, 0.85 * a));
        rt.FillEllipse(&el(r), br);
        br.SetColor(&color(1.0, 1.0, 1.0, 0.15 * a));
        rt.DrawEllipse(&el(r - 0.5 / self.s), br, 1.0 / self.s, None);
        // Beam L in a 64-unit box drawn 18 units wide.
        let k = 18.0 * q / 64.0;
        let (x0, y0) = (cx - 32.0 * k, cy - 32.0 * k);
        let rr = |x: f32, y: f32, w: f32, h: f32| D2D1_ROUNDED_RECT { rect: rect(x0 + x * k, y0 + y * k, w * k, h * k), radiusX: 4.75 * k, radiusY: 4.75 * k };
        br.SetColor(&color(1.0, 1.0, 1.0, a));
        rt.FillRoundedRectangle(&rr(15.0, 10.0, 9.5, 44.0), br);
        rt.FillRoundedRectangle(&rr(15.0, 44.5, 34.0, 9.5), br);
        rt.FillEllipse(&D2D1_ELLIPSE { point: D2D_POINT_2F { x: x0 + 44.0 * k, y: y0 + 21.0 * k }, radiusX: 5.25 * k, radiusY: 5.25 * k }, br);
        Ok(())
    }

    /// Grab the monitor under the island (the layered island is excluded without CAPTUREBLT),
    /// downscale, blur and saturate it once per open.
    unsafe fn capture(&mut self) {
        self.snap = None;
        self.snap_bmp = None;
        self.glass = None;
        if self.cfg.blur == 0 {
            return;
        }
        let (mw, mh) = (self.mon.right - self.mon.left, self.mon.bottom - self.mon.top);
        if mw <= 0 || mh <= 0 {
            return;
        }
        let d = (self.cfg.blur as f32 * self.s / 2.45).max(2.0);
        let (sw, sh) = (((mw as f32 / d) as i32).max(1), ((mh as f32 / d) as i32).max(1));
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: sw,
                biHeight: -sh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        if let Ok(bmp) = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            let old = SelectObject(mem, bmp);
            SetStretchBltMode(mem, HALFTONE);
            let ok = StretchBlt(mem, 0, 0, sw, sh, screen, self.mon.left, self.mon.top, mw, mh, SRCCOPY).as_bool();
            if ok && !bits.is_null() {
                let _ = GdiFlush();
                let mut px = std::slice::from_raw_parts(bits as *const u8, (sw * sh * 4) as usize).to_vec();
                blur_sat(&mut px, sw as usize, sh as usize, self.cfg.saturation as f32 / 100.0);
                self.snap_lum = backdrop_lum(&px, sw, sh, d, (self.org.0 - self.mon.left, self.org.1 - self.mon.top), self.px);
                self.snap = Some((sw, sh, px));
            }
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }

    /// Bitmap brush mapping the capture back onto the monitor, in window DIPs.
    unsafe fn snap_brush(&mut self, rt: &ID2D1DCRenderTarget) -> Result<Option<ID2D1BitmapBrush>> {
        let Some((sw, sh, px)) = &self.snap else { return Ok(None) };
        let (sw, sh) = (*sw, *sh);
        if self.snap_bmp.is_none() {
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            let b = rt.CreateBitmap(D2D_SIZE_U { width: sw as u32, height: sh as u32 }, Some(px.as_ptr() as _), (sw * 4) as u32, &props)?;
            self.snap_bmp = Some(b);
        }
        let mw = (self.mon.right - self.mon.left) as f32;
        let sc = mw / sw as f32 / self.s;
        let bp = D2D1_BITMAP_BRUSH_PROPERTIES { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR };
        let brp = D2D1_BRUSH_PROPERTIES {
            opacity: 1.0,
            transform: Matrix3x2 {
                M11: sc,
                M12: 0.0,
                M21: 0.0,
                M22: sc,
                M31: (self.mon.left - self.org.0) as f32 / self.s,
                M32: (self.mon.top - self.org.1) as f32 / self.s,
            },
        };
        Ok(Some(rt.CreateBitmapBrush(self.snap_bmp.as_ref(), Some(&bp), Some(&brp))?))
    }

    /// Tiled film grain, cached with the render target.
    unsafe fn noise(&mut self, rt: &ID2D1DCRenderTarget) -> Result<Option<ID2D1BitmapBrush>> {
        if self.noise_bmp.is_none() {
            let mut px = vec![0u8; 64 * 64 * 4];
            let mut x: u32 = 0x9e37_79b9;
            for p in px.chunks_mut(4) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let a = (x & 0xff) as f32 / 255.0 * 0.07;
                let v = if x & 0x100 != 0 { (255.0 * a) as u8 } else { 0 };
                p[0] = v;
                p[1] = v;
                p[2] = v;
                p[3] = (255.0 * a) as u8;
            }
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            self.noise_bmp = Some(rt.CreateBitmap(D2D_SIZE_U { width: 64, height: 64 }, Some(px.as_ptr() as _), 256, &props)?);
        }
        let bp = D2D1_BITMAP_BRUSH_PROPERTIES { extendModeX: D2D1_EXTEND_MODE_WRAP, extendModeY: D2D1_EXTEND_MODE_WRAP, interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR };
        let s = 1.0 / self.s;
        let brp = D2D1_BRUSH_PROPERTIES { opacity: 1.0, transform: Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: 0.0, M32: 0.0 } };
        Ok(Some(rt.CreateBitmapBrush(self.noise_bmp.as_ref(), Some(&bp), Some(&brp))?))
    }

    /// The baked glass as a brush (None without a backdrop). Rebuilt only when its inputs change.
    unsafe fn glass(&mut self, rt: &ID2D1DCRenderTarget, t: (f32, f32, f32, f32)) -> Result<Option<ID2D1BitmapBrush>> {
        if self.snap.is_none() {
            return Ok(None);
        }
        let key = [self.px.0 as u32, self.px.1 as u32, self.org.0 as u32, self.org.1 as u32, t.0.to_bits(), t.1.to_bits(), t.2.to_bits(), t.3.to_bits(), self.cfg.grain as u32];
        if self.glass.as_ref().map_or(true, |(k, _)| *k != key) {
            let Some(sb) = self.snap_brush(rt)? else { return Ok(None) };
            let (w, h) = (self.px.0 as f32 / self.s, self.px.1 as f32 / self.s);
            let brt = rt.CreateCompatibleRenderTarget(
                Some(&D2D_SIZE_F { width: w, height: h }),
                Some(&D2D_SIZE_U { width: self.px.0 as u32, height: self.px.1 as u32 }),
                None,
                D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE,
            )?;
            let all = rect(0.0, 0.0, w, h);
            brt.BeginDraw();
            brt.FillRectangle(&all, &sb);
            let tb = brt.CreateSolidColorBrush(&color(t.0, t.1, t.2, t.3), None)?;
            brt.FillRectangle(&all, &tb);
            if self.cfg.grain {
                if let Some(nb) = self.noise(rt)? {
                    brt.FillRectangle(&all, &nb);
                }
            }
            brt.EndDraw(None, None)?;
            self.glass = Some((key, brt.GetBitmap()?));
        }
        let bp = D2D1_BITMAP_BRUSH_PROPERTIES { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR };
        Ok(Some(rt.CreateBitmapBrush(self.glass.as_ref().map(|(_, b)| b), Some(&bp), None)?))
    }

    unsafe fn draw(&mut self, rt: &ID2D1DCRenderTarget) -> Result<()> {
        let (ext, hh, k) = self.shape();
        if ext < 0.5 {
            return Ok(());
        }
        let br = rt.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 1.0), None)?;
        // Peek is the logo orb; the glass panel fades in over it as the island opens.
        let panel = (k / 0.25).clamp(0.0, 1.0);
        if panel < 1.0 {
            self.orb(rt, &br, (ext / PEEK_EXT).min(1.0), 1.0 - panel)?;
        }
        if panel <= 0.0 {
            return Ok(());
        }
        let geo = self.geometry(ext, hh, k)?;
        let (r, g, b, ta) = self.tint();
        let glass = self.glass(rt, (r, g, b, ta))?;
        if let Some(gb) = &glass {
            gb.SetOpacity(panel);
            rt.FillGeometry(&geo, gb, None::<&ID2D1Brush>);
        } else {
            // Without a backdrop a see-through tint would show the desktop sharp; keep it readable.
            let ta = if self.cfg.blur > 0 { ta.max(0.85) } else { ta };
            br.SetColor(&color(r, g, b, ta * panel));
            rt.FillGeometry(&geo, &br, None::<&ID2D1Brush>);
        }
        let (l, t, rr, bb) = self.bounds(ext, hh);
        // The front Spotify cover, blurred into soft ambient colour fields behind the panel (static), when the tint is on.
        if self.cov_bmp.is_none() {
            if let Some(px) = &self.cov_px {
                let props = D2D1_BITMAP_PROPERTIES { pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED }, dpiX: 96.0, dpiY: 96.0 };
                self.cov_bmp = rt.CreateBitmap(D2D_SIZE_U { width: 48, height: 48 }, Some(px.as_ptr() as *const _), 48 * 4, &props).ok();
            }
        }
        let cover = if self.cov_cur.is_some() { self.cov_bmp.clone() } else { None };
        if let Some(bmp) = &cover {
            let (w, h) = (rr - l, bb - t);
            let sz = bmp.GetSize();
            let k = (w / sz.width.max(1.0)).max(h / sz.height.max(1.0));
            let bp = D2D1_BITMAP_BRUSH_PROPERTIES { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR };
            let brp = D2D1_BRUSH_PROPERTIES { opacity: panel, transform: Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: l + (w - sz.width * k) / 2.0, M32: t + (h - sz.height * k) / 2.0 } };
            let ab = rt.CreateBitmapBrush(bmp, Some(&bp), Some(&brp))?;
            rt.FillGeometry(&geo, &ab, None::<&ID2D1Brush>);
            // Dark scrim, heavier toward the bottom, so text and controls stay readable on any cover.
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: color(0.0, 0.0, 0.0, 0.3 * panel) },
                D2D1_GRADIENT_STOP { position: 1.0, color: color(0.0, 0.0, 0.0, 0.5 * panel) },
            ];
            let col = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
            let sb = rt.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: D2D_POINT_2F { x: l, y: t }, endPoint: D2D_POINT_2F { x: l, y: bb } },
                None,
                &col,
            )?;
            rt.FillGeometry(&geo, &sb, None::<&ID2D1Brush>);
        } else if let Some(p) = self.cov_cur {
            // No cover bitmap yet: soft colour pools from the palette.
            let (w, h) = (rr - l, bb - t);
            for (i, (fx, fy, rad, al)) in [(0.10f32, 0.25f32, 0.85f32, 0.65f32), (0.95, 1.0, 0.8, 0.6), (0.9, 0.0, 0.55, 0.45)].into_iter().enumerate() {
                let c = (p[3 + i * 3], p[4 + i * 3], p[5 + i * 3]);
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: color(c.0, c.1, c.2, al * panel) },
                    D2D1_GRADIENT_STOP { position: 1.0, color: color(c.0, c.1, c.2, 0.0) },
                ];
                let col = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
                let gb = rt.CreateRadialGradientBrush(
                    &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: D2D_POINT_2F { x: l + w * fx, y: t + h * fy }, gradientOriginOffset: D2D_POINT_2F { x: 0.0, y: 0.0 }, radiusX: w.max(h) * rad, radiusY: w.max(h) * rad * 0.8 },
                    None,
                    &col,
                )?;
                rt.FillGeometry(&geo, &gb, None::<&ID2D1Brush>);
            }
        }
        if self.cfg.sheen {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: color(1.0, 1.0, 1.0, 0.12 * panel) },
                D2D1_GRADIENT_STOP { position: 1.0, color: color(1.0, 1.0, 1.0, 0.0) },
            ];
            let col = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
            let lb = rt.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: D2D_POINT_2F { x: l, y: t }, endPoint: D2D_POINT_2F { x: l, y: t + (bb - t) * 0.38 } },
                None,
                &col,
            )?;
            rt.FillGeometry(&geo, &lb, None::<&ID2D1Brush>);
        }
        if self.cfg.grain && glass.is_none() {
            if let Some(nb) = self.noise(rt)? {
                nb.SetOpacity(panel);
                rt.FillGeometry(&geo, &nb, None::<&ID2D1Brush>);
            }
        }
        if self.cfg.edge > 0.0 {
            br.SetColor(&color(1.0, 1.0, 1.0, self.cfg.edge * panel));
            rt.DrawGeometry(&geo, &br, 1.0 / self.s, None);
        }
        // Pick text colour from what the eye sees: the tint laid over the blurred backdrop.
        let tl = 0.299 * r + 0.587 * g + 0.114 * b;
        let lum = if glass.is_some() { tl * ta + self.snap_lum * (1.0 - ta) } else if self.cfg.tint == "clear" { 0.0 } else { tl };
        let light = lum > 0.58;
        let fg = if light { (0.06, 0.06, 0.07) } else { (1.0, 1.0, 1.0) };
        let a = ((k - 0.45) / 0.45).clamp(0.0, 1.0);
        let alpha = a * a * (3.0 - 2.0 * a);
        if alpha < 0.01 {
            return Ok(());
        }
        rt.PushAxisAlignedClip(&rect(l, t, rr - l, bb - t), D2D1_ANTIALIAS_MODE_ALIASED);
        let (ox, oy) = self.co();
        if self.cfg.animation != "none" && alpha < 1.0 {
            let z = 0.88 + 0.12 * alpha;
            let (bw, bh) = self.cb();
            let (cx, cy) = (ox + bw / 2.0, oy + bh / 2.0);
            rt.SetTransform(&Matrix3x2 { M11: z, M12: 0.0, M21: 0.0, M22: z, M31: cx * (1.0 - z), M32: cy * (1.0 - z) });
        }
        let res = self.content(rt, &br, fg, alpha, ox, oy);
        rt.SetTransform(&Matrix3x2::identity());
        rt.PopAxisAlignedClip();
        res
    }

    /// Shell icon for a path as a bitmap, cached. None when the shell has no icon.
    unsafe fn icon(&mut self, rt: &ID2D1DCRenderTarget, path: &str, img: bool) -> Option<ID2D1Bitmap> {
        if let Some(b) = self.icons.get(path) {
            return b.clone();
        }
        if self.icons.len() > 300 {
            self.icons.clear();
        }
        if self.wic.is_none() {
            self.wic = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok();
        }
        let bmp = self.wic.as_ref().and_then(|wic| if img { thumb(rt, wic, path) } else { shell_icon(rt, wic, path) });
        self.icons.insert(path.to_string(), bmp.clone());
        bmp
    }

    /// Decoded cover for a session, cached per id and regenerated when its art generation changes.
    unsafe fn art_for(&mut self, rt: &ID2D1DCRenderTarget, id: &str, gen: u32, bytes: Option<Arc<Vec<u8>>>) -> Option<ID2D1Bitmap> {
        if let Some((g, b)) = self.arts.get(id) {
            if *g == gen {
                return b.clone();
            }
        }
        if self.arts.len() > 8 {
            self.arts.clear();
        }
        if self.wic.is_none() {
            self.wic = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok();
        }
        let bmp = match (&self.wic, bytes) {
            (Some(wic), Some(b)) => decode(rt, wic, &b),
            _ => None,
        };
        self.arts.insert(id.to_string(), (gen, bmp.clone()));
        bmp
    }

    unsafe fn content(&mut self, rt: &ID2D1DCRenderTarget, br: &ID2D1SolidColorBrush, fg: (f32, f32, f32), alpha: f32, ox: f32, oy: f32) -> Result<()> {
        let fo = self.fonts.clone().unwrap();
        let sessions: Vec<media::Media> = self.worker.as_ref().map(|w| w.state.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap_or_default();
        let c = Ctx { rt, br, fg, acc: self.cov_cur.map(|p| (p[12], p[13], p[14])), alpha, ox, oy };
        let (bw, _) = self.cb();

        // Tab bar.
        let tabs = self.tabs();
        let style = self.cfg.tab_style.as_str();
        if style == "segmented" {
            if let (Some(f), Some(l)) = (tabs.first(), tabs.last()) {
                c.rrect(f.0 - 3.0, 11.0, l.0 + l.1 - f.0 + 6.0, 34.0, 10.0, 0.07);
            }
        }
        for &(x, w, tab) in &tabs {
            let on = self.tab == tab;
            let hot = self.hover == Hover::Tab(tab);
            let r = match style {
                "underline" => {
                    if on || hot {
                        if on { c.rrect_acc(x + 8.0, 40.0, w - 16.0, 2.0, 1.0, 0.95) } else { c.rrect(x + 8.0, 40.0, w - 16.0, 2.0, 1.0, 0.3) }
                    }
                    None
                }
                "segmented" => Some(7.0),
                _ => Some(TAB_R),
            };
            if let Some(r) = r {
                if on || hot {
                    if on { c.rrect_acc(x, 14.0, w, 28.0, r, if c.acc.is_some() { 0.32 } else { 0.16 }) } else { c.rrect(x, 14.0, w, 28.0, r, 0.08) }
                }
            }
            let a = if on { 1.0 } else if hot { 0.8 } else { 0.55 };
            if self.cfg.tab_icons {
                c.text(tab.glyph(), &fo.icon, x, 14.0, w, 28.0, a);
            } else {
                c.text(tab.label(), &fo.center, x, 14.0, w, 28.0, a);
            }
            if tab == Tab::Media && sessions.iter().any(|s| s.playing) {
                if self.cfg.tab_icons { c.circle(x + w - 5.0, 18.0, 2.5, 0.9) } else { c.circle(x + w - 9.0, 21.0, 3.0, 0.9) }
            }
        }
        // Thin divider under the header.
        c.rrect(PAD, 49.0, bw - 2.0 * PAD, 1.0, 0.5, 0.1);

        // Body slides in from the side of the tab that was picked while fading up.
        let te = { let t = self.tab_a; 1.0 - (1.0 - t).powi(3) };
        let alpha = alpha * te;
        let ox = ox + (1.0 - te) * 28.0 * self.tab_dir;
        let c = Ctx { rt, br, fg, acc: self.cov_cur.map(|p| (p[12], p[13], p[14])), alpha, ox, oy };

        if self.tab == Tab::Clip {
            let cw = bw - 2.0 * PAD;
            // Filter strip: chips on the left, Clear on the right.
            if self.cfg.clip_tools {
                let mut x = PAD;
                for (id, label, w) in self.chips() {
                    let on = self.filter == id;
                    if on {
                        c.rrect(x, CHIP_Y, w, CHIP_H, 8.0, 0.16);
                    } else if self.hover == Hover::Chip(id) {
                        c.rrect(x, CHIP_Y, w, CHIP_H, 8.0, 0.08);
                    }
                    c.text(label, &fo.center, x, CHIP_Y, w, CHIP_H, if on { 1.0 } else { 0.55 });
                    x += w + 4.0;
                }
            }
            if self.hover == Hover::Clear {
                c.rrect(bw - PAD - 46.0, CHIP_Y, 46.0, CHIP_H, 8.0, 0.1);
            }
            c.text("Clear", &fo.small, bw - PAD - 46.0, CHIP_Y, 46.0, CHIP_H, 0.6);
            let view = self.view();
            let sf = self.scroll_f.max(0.0);
            let first = sf.floor() as usize;
            let frac = sf - first as f32;
            let icons: Vec<Option<ID2D1Bitmap>> = (0..self.rows() + 1)
                .map(|i| {
                    let e = self.store.items.get(*view.get(first + i)?)?;
                    if !e.files {
                        return None;
                    }
                    let path = e.s.lines().next()?.to_string();
                    self.icon(rt, &path, e.img)
                })
                .collect();
            let items = &self.store.items;
            if view.is_empty() {
                let (g, t, s) = match (items.is_empty(), self.filter) {
                    (true, _) => ("\u{E8C8}", "Nothing copied yet", "Copy text or files and they show up here"),
                    (_, 4) => ("\u{E718}", "Nothing pinned", "Hover an item and tap the pin"),
                    _ => ("\u{E71C}", "Nothing here", "No items match this filter"),
                };
                c.text(g, &fo.icon_big, PAD, 100.0, cw, 36.0, 0.3);
                c.text(t, &fo.center, PAD, 140.0, cw, 24.0, 0.7);
                c.text(s, &fo.center, PAD, 166.0, cw, 18.0, 0.4);
            }
            let lh = ROW_PITCH * self.rows() as f32;
            rt.PushAxisAlignedClip(&rect(ox, oy + LIST_Y - 4.0, bw, lh + 4.0), D2D1_ANTIALIAS_MODE_ALIASED);
            for i in 0..self.rows() + 1 {
                let Some(&idx) = view.get(first + i) else { break };
                let Some(e) = items.get(idx) else { break };
                let y = LIST_Y + ROW_PITCH * (i as f32 - frac);
                let hot = matches!(self.hover, Hover::Row(h) | Hover::Pin(h) | Hover::Del(h) if h == idx);
                let tools = self.cfg.clip_tools;
                let flashing = matches!(self.flash, Some((fi, _)) if fi == idx);
                let lifted = idx == self.row_i && alpha >= 1.0 && (self.row_t > 0.0 || self.press_t > 0.0);
                if lifted {
                    let e = |t: f32| t * t * (3.0 - 2.0 * t);
                    let z = 1.0 - (1.0 - PRESS) * e(self.press_t);
                    let (cx, cy) = (ox + PAD + cw / 2.0, oy + y + ROW_H / 2.0);
                    rt.SetTransform(&Matrix3x2 { M11: z, M12: 0.0, M21: 0.0, M22: z, M31: cx * (1.0 - z), M32: cy * (1.0 - z) - LIFT * e(self.row_t) });
                }
                c.rrect(PAD, y, cw, ROW_H, 16.0, if flashing { 0.18 } else if hot { 0.14 } else { 0.06 });
                if hot {
                    c.rrect(PAD, y + 12.0, 3.0, 18.0, 1.5, 0.9);
                }
                let glyph = if flashing { "\u{E73E}" } else if e.files { "\u{E8A5}" } else { "\u{E8C8}" };
                c.rrect(PAD + 10.0, y + 6.0, 30.0, 30.0, 12.0, if flashing { 0.22 } else { 0.09 });
                match (&icons[i], flashing) {
                    (Some(bmp), false) if e.img => {
                        // Thumbnail: fit inside the 30px tile, keep aspect.
                        let s = bmp.GetSize();
                        let k = 26.0 / s.width.max(s.height);
                        let (tw, th) = (s.width * k, s.height * k);
                        let d = rect(ox + PAD + 25.0 - tw / 2.0, oy + y + 21.0 - th / 2.0, tw, th);
                        rt.DrawBitmap(bmp, Some(&d), alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                    }
                    (Some(bmp), false) => {
                        let d = rect(ox + PAD + 13.0, oy + y + 9.0, 24.0, 24.0);
                        rt.DrawBitmap(bmp, Some(&d), alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                    }
                    _ => c.text(glyph, &fo.icon, PAD + 10.0, y + 6.0, 30.0, 30.0, if flashing { 1.0 } else { 0.7 }),
                }
                let sz = if e.files { size_label(e) } else { String::new() };
                let lw = if sz.is_empty() { cw - 50.0 - 52.0 } else { cw - 50.0 - 52.0 - 64.0 };
                c.text(&row_label(e), &fo.body, PAD + 50.0, y, lw, ROW_H, 1.0);
                if !sz.is_empty() {
                    let x = if hot && !flashing { cw - 98.0 - 64.0 - if tools { 6.0 } else { 0.0 } } else { cw - 48.0 - 64.0 };
                    c.text(&sz, &fo.small_r, PAD + x, y, 60.0, ROW_H, 0.45);
                }
                if hot && !flashing && tools {
                    // Pin and delete buttons replace the age on hover.
                    for (bx, glyph, h, a) in [(PIN_X, if e.pin { "\u{E77A}" } else { "\u{E718}" }, Hover::Pin(idx), if e.pin { 1.0 } else { 0.7 }), (DEL_X, "\u{E74D}", Hover::Del(idx), 0.7)] {
                        let bx = cw - bx;
                        if self.hover == h {
                            c.rrect(PAD + bx, y + 9.0, 24.0, 24.0, 8.0, 0.16);
                        }
                        c.text(glyph, &fo.icon, PAD + bx, y + 9.0, 24.0, 24.0, if self.hover == h { 1.0 } else { a });
                    }
                } else if hot && !flashing {
                    c.text("Click to copy", &fo.small_r, PAD + cw - 98.0, y, 90.0, ROW_H, 0.55);
                } else {
                    if e.pin {
                        c.text("\u{E718}", &fo.icon, PAD + cw - 66.0, y, 18.0, ROW_H, 0.8);
                    }
                    c.text(&age(e.t), &fo.small_r, PAD + cw - 48.0, y, 40.0, ROW_H, 0.45);
                }
                if lifted {
                    rt.SetTransform(&Matrix3x2::identity());
                }
            }
            rt.PopAxisAlignedClip();
            return Ok(());
        }

        if self.tab == Tab::Sound {
            let (th, cols, bx) = self.sound_lay();
            let mix = std::mem::take(&mut self.mix);
            // Vertical track: rounded capsule, fill grows up from a circular base holding the glyph.
            let track = |x: f32, w: f32, h: f32, v: f32, mute: bool, hot: bool| {
                c.rrect(x, SND_Y, w, h, w / 2.0, if hot { 0.18 } else { 0.12 });
                let fh = w + (h - w) * v;
                c.rrect(x, SND_Y + h - fh, w, fh, w / 2.0, if mute { 0.35 } else { 0.92 });
            };
            for n in 0..cols.min(mix.len() + 1) {
                let cx = self.col_x(n);
                let (v, mute, name) = if n == 0 { (self.master.0, self.master.1, "Master") } else { (mix[n - 1].vol, mix[n - 1].mute, mix[n - 1].name.as_str()) };
                let tx = cx + (COL_W - 36.0) / 2.0;
                track(tx, 36.0, th, v, mute, self.hover == Hover::Vol(n) || self.slide == Some(n));
                let gy = SND_Y + th - 36.0;
                if n == 0 {
                    c.ink(if mute { "\u{E74F}" } else { "\u{E767}" }, &fo.icon, tx, gy + 9.0, 36.0, 20.0, 0.85);
                } else {
                    match self.icon(rt, &mix[n - 1].path, false) {
                        Some(bmp) => rt.DrawBitmap(&bmp, Some(&rect(ox + tx + 8.0, oy + gy + 8.0, 20.0, 20.0)), alpha * if mute { 0.5 } else { 1.0 }, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
                        None => c.ink("\u{E8D6}", &fo.icon, tx, gy + 9.0, 36.0, 20.0, 0.85),
                    }
                }
                // Mute button under the track.
                let my = SND_Y + th + 6.0;
                let hot = self.hover == Hover::Mute(n);
                c.rrect(cx + (COL_W - MUTE_D) / 2.0, my, MUTE_D, MUTE_D, MUTE_D / 2.0, if mute { 0.22 } else if hot { 0.14 } else { 0.06 });
                c.text(if mute { "\u{E74F}" } else { "\u{E767}" }, &fo.icon, cx, my + 5.0, COL_W, 18.0, if mute || hot { 1.0 } else { 0.6 });
                c.text(name, &fo.center, cx - 2.0, my + MUTE_D + 3.0, COL_W + 4.0, 16.0, 0.6);
                if n == 0 {
                    c.rrect(cx + COL_W + 7.0, SND_Y + 8.0, 1.0, th + MUTE_D - 10.0, 0.5, 0.12);
                }
            }
            self.mix = mix;
            // "+" column: pin another app so it always shows.
            let ax = self.col_x(cols) + (COL_W - 36.0) / 2.0;
            let hot = self.hover == Hover::Add;
            c.rrect(ax, SND_Y, 36.0, th, 18.0, if hot { 0.12 } else { 0.05 });
            c.text("\u{E710}", &fo.icon, ax, SND_Y + th / 2.0 - 10.0, 36.0, 20.0, if hot { 1.0 } else { 0.5 });
            c.text("Add", &fo.center, ax - 10.0, SND_Y + th + 6.0 + MUTE_D + 3.0, 56.0, 16.0, if hot { 0.9 } else { 0.5 });
            // Brightness: one iOS-style pill per monitor, left to right.
            let ph = th + MUTE_D + 6.0;
            let n = self.bri.len().max(1);
            for i in 0..n {
                let x = Self::pill_x(bx, i);
                let hot = self.hover == Hover::Bri(i) || self.slide == Some(usize::MAX - i);
                let label = if n > 1 { format!("{}", i + 1) } else { String::new() };
                match self.bri.get(i).copied().flatten() {
                    Some(v) => {
                        track(x, BRI_W, ph, v, false, hot);
                        c.ink("\u{E706}", &fo.icon_big, x, SND_Y + ph - BRI_W / 2.0 - 14.0, BRI_W, 28.0, 0.85);
                        let t = if n > 1 { format!("{label} \u{00B7} {:.0}%", v * 100.0) } else { format!("{:.0}%", v * 100.0) };
                        c.text(&t, &fo.center, x - 6.0, SND_Y + ph + 3.0, BRI_W + 12.0, 16.0, 0.6);
                    }
                    None => {
                        c.rrect(x, SND_Y, BRI_W, ph, BRI_W / 2.0, 0.06);
                        c.text("\u{E706}", &fo.icon_big, x, SND_Y + ph - BRI_W / 2.0 - 14.0, BRI_W, 28.0, 0.3);
                        let t = if self.bri.is_empty() { "...".to_string() } else if n > 1 { format!("{label} \u{00B7} N/A") } else { "N/A".into() };
                        c.text(&t, &fo.center, x - 6.0, SND_Y + ph + 3.0, BRI_W + 12.0, 16.0, 0.4);
                    }
                }
            }
            return Ok(());
        }

        if self.tab == Tab::System {
            if self.sys.is_none() {
                self.sample_sys();
            }
            let Some(s) = self.sys else { return Ok(()) };
            let cw = bw - 2.0 * PAD;
            let up = s.uptime_s;
            let up = if up >= 86400 { format!("{}d {}h", up / 86400, up / 3600 % 24) } else { format!("{}h {}m", up / 3600, up / 60 % 60) };
            let disk = 1.0 - s.disk_free as f32 / s.disk_total.max(1) as f32;
            // (fraction, label, detail, warn)
            let mut g: Vec<(f32, &str, String, bool)> = vec![
                (s.cpu, "CPU", "Usage".into(), s.cpu > 0.9),
                (s.ram, "Memory", if cw / 4.0 < 90.0 { format!("{} used", gb(s.ram_used)) } else { format!("{} / {}", gb(s.ram_used), gb(s.ram_total)) }, s.ram > 0.9),
                (disk, "Disk C:", format!("{} free", gb(s.disk_free)), disk > 0.9),
            ];
            if let Some((p, chg)) = s.bat {
                let f = p as f32 / 100.0;
                g.push((f, "Battery", if chg { "Charging".into() } else { "On battery".into() }, f < 0.2 && !chg));
            }
            // One row of gauges, side by side.
            let slot = cw / g.len() as f32;
            let r = (slot / 2.0 - 10.0).min(34.0);
            let cy = 68.0 + r;
            for (i, (f, label, detail, warn)) in g.iter().enumerate() {
                let x = PAD + slot * i as f32;
                let cx = x + slot / 2.0;
                c.ring(cx, cy, r, 12.0, *f, 1.0, *warn);
                c.text(&format!("{:.0}%", f * 100.0), &fo.num, x, cy - 12.0, slot, 24.0, 0.95);
                c.text(label, &fo.center, x, cy + r + 8.0, slot, 18.0, 0.85);
                c.text(detail, &fo.center, x + 2.0, cy + r + 26.0, slot - 4.0, 16.0, 0.45);
            }
            let y = cy + r + 52.0;
            c.rrect(PAD, y, cw, 34.0, 12.0, 0.06);
            c.text("\u{E823}", &fo.icon, PAD + 8.0, y, 24.0, 34.0, 0.75);
            c.text("Uptime", &fo.body, PAD + 38.0, y, cw - 170.0, 34.0, 0.9);
            c.text(&up, &fo.small_r, PAD + cw - 140.0, y, 130.0, 34.0, 0.6);
            return Ok(());
        }

        if self.picking() {
            let cw = bw - 2.0 * PAD;
            let hot = self.hover == Hover::Back;
            c.rrect(PAD, 56.0, 32.0, 32.0, 10.0, if hot { 0.14 } else { 0.06 });
            c.text("\u{E72B}", &fo.icon, PAD, 56.0, 32.0, 32.0, if hot { 1.0 } else { 0.7 });
            c.text(if self.pick_mix { "Pin an app" } else { "Add an app" }, &fo.title, PAD + 44.0, 56.0, 150.0, 32.0, 0.95);
            // Search box: typing goes here while the picker is open.
            let (sx, sw) = (PAD + 200.0, cw - 200.0);
            c.rrect(sx, 58.0, sw, 28.0, 14.0, 0.08);
            c.text("\u{E721}", &fo.icon, sx + 6.0, 58.0, 22.0, 28.0, 0.5);
            if self.pick_q.is_empty() {
                c.text("Search", &fo.body, sx + 32.0, 58.0, sw - 40.0, 28.0, 0.4);
            } else {
                c.text(&format!("{}|", self.pick_q), &fo.body, sx + 32.0, 58.0, sw - 40.0, 28.0, 0.95);
            }
            if self.apps.is_none() {
                c.text(if self.pick_mix { "Finding audio apps..." } else { "Finding apps..." }, &fo.center, PAD, PICK_Y + 20.0, cw, 20.0, 0.6);
                return Ok(());
            }
            if self.pick_hits.is_empty() {
                c.text("No matches", &fo.center, PAD, PICK_Y + 20.0, cw, 20.0, 0.6);
            }
            let rows = self.pick_rows();
            let total = self.pick_hits.len();
            for k in self.pick_scroll..total.min(self.pick_scroll + rows) {
                // Only the visible rows are cloned (icon() needs &mut self).
                let Some((name, path)) = self.apps.as_ref().and_then(|a| a.get(self.pick_hits[k])).cloned() else { continue };
                let (name, path) = (&name, &path);
                let y = PICK_Y + (k - self.pick_scroll) as f32 * PICK_H;
                let hot = self.hover == Hover::App(k);
                if hot {
                    c.rrect(PAD, y, cw, PICK_H - 4.0, 10.0, 0.12);
                }
                let bmp = if self.pick_mix { None } else { self.icon(rt, path, false) };
                match bmp {
                    Some(bmp) => rt.DrawBitmap(&bmp, Some(&rect(ox + PAD + 8.0, oy + y + 6.0, 24.0, 24.0)), alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
                    None => c.text(if self.pick_mix { "\u{E767}" } else { "\u{E8A5}" }, &fo.icon, PAD + 8.0, y, 24.0, PICK_H - 4.0, 0.6),
                }
                c.text(name, &fo.body, PAD + 42.0, y, cw - 50.0, PICK_H - 4.0, if hot { 1.0 } else { 0.85 });
            }
            // Thin scroll indicator when the list overflows.
            if total > rows {
                let track = rows as f32 * PICK_H - 4.0;
                let th = (track * rows as f32 / total as f32).max(16.0);
                let ty = PICK_Y + (track - th) * self.pick_scroll as f32 / (total - rows) as f32;
                c.rrect(bw - PAD + 8.0, ty, 3.0, th, 1.5, 0.3);
            }
            return Ok(());
        }

        if self.tab == Tab::Shelf {
            // Sticky notes: "+" card first, then one card per note.
            let n = self.items.notes.len().min(self.note_cap());
            let (ax, ay, nw, nh) = self.note_rect(0);
            let hot = self.hover == Hover::Add;
            c.rrect(ax, ay, nw, nh, 14.0, if hot { 0.14 } else { 0.06 });
            c.text("\u{E710}", &fo.icon_big, ax, ay + nh / 2.0 - 26.0, nw, 36.0, if hot { 1.0 } else { 0.6 });
            c.text("New note", &fo.center, ax, ay + nh / 2.0 + 10.0, nw, 18.0, if hot { 1.0 } else { 0.7 });
            for i in 0..n {
                let (x, y, w, h) = self.note_rect(i + 1);
                let hot = self.hover == Hover::Item(i);
                let editing = self.edit == Some(i);
                c.rrect(x, y, w, h, 14.0, if editing { 0.18 } else if hot { 0.13 } else { 0.08 });
                let t = &self.items.notes[i];
                if t.is_empty() && !editing {
                    c.text("Empty note", &fo.small, x + 12.0, y + 10.0, w - 24.0, h - 20.0, 0.4);
                } else {
                    let s = if editing { format!("{t}|") } else { t.clone() };
                    c.text(&s, &fo.small, x + 12.0, y + 10.0, w - 24.0, h - 20.0, 0.9);
                }
            }
            if n == 0 {
                let cw = bw - 2.0 * PAD;
                c.text("Click a note to type, right-click to delete", &fo.center, PAD, ay + nh + 12.0, cw, 18.0, 0.4);
            }
            return Ok(());
        }

        if self.tab == Tab::Launch {
            let cw = bw - 2.0 * PAD;
            let n = self.shown();
            // "+" tile: opens a file picker.
            let (ax, ay) = self.tile_xy(n);
            let hot = self.hover == Hover::Add;
            c.rrect(ax, ay, TILE, TILE, 14.0, if hot { 0.14 } else { 0.06 });
            c.text("\u{E710}", &fo.icon_big, ax, ay + 8.0, TILE, 36.0, if hot { 1.0 } else { 0.6 });
            c.text("Add", &fo.center, ax + 4.0, ay + 48.0, TILE - 8.0, 18.0, if hot { 1.0 } else { 0.7 });
            if self.items.launch.is_empty() {
                c.text("Click + or drop apps here", &fo.center, PAD, ay + TILE + 16.0, cw, 20.0, 0.7);
                c.text("Right-click a tile to remove it", &fo.center, PAD, ay + TILE + 38.0, cw, 18.0, 0.4);
            }
            for i in 0..n {
                let p = &self.items.launch[i].clone();
                let (x, y) = self.tile_xy(i);
                let hot = self.hover == Hover::Item(i);
                c.rrect(x, y, TILE, TILE, 14.0, if hot { 0.14 } else { 0.06 });
                match self.icon(rt, p, false) {
                    Some(bmp) => rt.DrawBitmap(&bmp, Some(&rect(ox + x + 20.0, oy + y + 10.0, 32.0, 32.0)), alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
                    None => c.text("\u{E8A5}", &fo.icon_big, x, y + 8.0, TILE, 36.0, 0.6),
                }
                c.text(file_name(p), &fo.center, x + 4.0, y + 48.0, TILE - 8.0, 18.0, if hot { 1.0 } else { 0.7 });
            }
            return Ok(());
        }

        if let Some((bright, v, t)) = self.hud {
            let a = (1.4 - t.elapsed().as_secs_f32()).clamp(0.0, 0.3) / 0.3;
            let (hx, hy) = (bw - PAD - 150.0, 14.0);
            let cc = Ctx { rt, br, fg, acc: self.cov_cur.map(|p| (p[12], p[13], p[14])), alpha: alpha * a, ox, oy };
            cc.rrect(hx, hy, 150.0, 28.0, 14.0, 0.14);
            cc.text(if bright { "\u{E706}" } else if v <= 0.0 { "\u{E74F}" } else { "\u{E767}" }, &fo.icon, hx + 4.0, hy, 24.0, 28.0, 0.9);
            cc.rrect(hx + 32.0, hy + 12.0, 80.0, 4.0, 2.0, 0.2);
            cc.rrect(hx + 32.0, hy + 12.0, 80.0 * v, 4.0, 2.0, 0.95);
            cc.text(&format!("{}%", (v * 100.0).round()), &fo.small_r, hx + 116.0, hy + 6.0, 30.0, 16.0, 0.7);
        }
        // Media tab: every playing session gets an equal player, the rest are small tiles.
        if sessions.is_empty() {
            self.anim.clear();
            c.text("\u{E8D6}", &fo.icon_big, PAD, 90.0, bw - 2.0 * PAD, 36.0, 0.3);
            c.text("Nothing playing", &fo.center, PAD, 130.0, bw - 2.0 * PAD, 24.0, 0.7);
            c.text("Play music or video in any app or browser", &fo.center, PAD, 156.0, bw - 2.0 * PAD, 18.0, 0.4);
            return Ok(());
        }
        let now = Instant::now();
        let dt = now.duration_since(self.anim_t).as_secs_f32().min(0.05);
        self.anim_t = now;
        self.animating = false;
        self.anim.retain(|id, _| sessions.iter().any(|s| &s.id == id));
        let np = self.players.max(1).min(sessions.len());
        for (i, s) in sessions.iter().enumerate().take(np + self.tiles) {
            let (sx, sy) = if i < np {
                let l = self.lay_n(i);
                (l.art.0, l.art.1)
            } else {
                let t = self.tile(i - np);
                (t.0, t.1)
            };
            let (dx, dy, a) = self.ease(&s.id, sx, sy, dt);
            let cc = Ctx { rt, br, fg, acc: self.cov_cur.map(|p| (p[12], p[13], p[14])), alpha: alpha * a, ox: ox + dx, oy: oy + dy };
            if i < np {
                self.player(rt, &cc, &fo, s, i);
            } else {
                self.tile_card(&cc, &fo, s, i - np);
            }
        }
        Ok(())
    }

    /// Eases a card's slot origin toward (tx, ty); returns (offset x, offset y, alpha). New cards slide and fade in.
    fn ease(&mut self, id: &str, tx: f32, ty: f32, dt: f32) -> (f32, f32, f32) {
        let inst = self.instant();
        let off = if self.side == Side::Top { (24.0, 0.0) } else { (0.0, 24.0) };
        let e = self.anim.entry(id.to_string()).or_insert(if inst { (tx, ty, 1.0) } else { (tx + off.0, ty + off.1, 0.0) });
        if inst {
            *e = (tx, ty, 1.0);
        } else {
            let k = 1.0 - (-dt * 14.0).exp();
            e.0 += (tx - e.0) * k;
            e.1 += (ty - e.1) * k;
            e.2 += (1.0 - e.2) * k;
            if (tx - e.0).abs() < 0.3 && (ty - e.1).abs() < 0.3 && e.2 > 0.99 {
                *e = (tx, ty, 1.0);
            }
        }
        let (cx, cy, a) = *e;
        if cx != tx || cy != ty || a < 1.0 {
            self.animating = true;
        }
        (cx - tx, cy - ty, a)
    }

    /// One full player: cover, title, scrub bar and transport controls.
    unsafe fn player(&mut self, rt: &ID2D1DCRenderTarget, c: &Ctx, fo: &Fonts, p: &media::Media, i: usize) {
        let l = self.lay_n(i);
        let (ox, oy, alpha) = (c.ox, c.oy, c.alpha);
        let art = self.art_for(rt, &p.id, p.art_gen, p.art.clone());
        c.rrect(l.art.0 - 4.0, l.art.1 - 4.0, l.art.2 + 8.0, l.art.2 + 8.0, 18.0, 0.07);
        match &art {
            Some(bmp) => {
                let d = rect(ox + l.art.0, oy + l.art.1, l.art.2, l.art.2);
                // Cover-fit: scale the short side to the square and centre-crop the long side (no stretch).
                let sz = bmp.GetSize();
                let k = l.art.2 / sz.width.min(sz.height).max(1.0);
                let (dx, dy) = ((l.art.2 - sz.width * k) / 2.0, (l.art.2 - sz.height * k) / 2.0);
                let bp = D2D1_BITMAP_BRUSH_PROPERTIES { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR };
                let brp = D2D1_BRUSH_PROPERTIES { opacity: alpha, transform: Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: d.left + dx, M32: d.top + dy } };
                match rt.CreateBitmapBrush(bmp, Some(&bp), Some(&brp)) {
                    Ok(b) => rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: d, radiusX: 14.0, radiusY: 14.0 }, &b),
                    Err(_) => rt.DrawBitmap(bmp, Some(&d), alpha, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
                }
            }
            None => {
                c.rrect(l.art.0, l.art.1, l.art.2, l.art.2, 16.0, 0.08);
                c.text("\u{E8D6}", &fo.icon_big, l.art.0, l.art.1, l.art.2, l.art.2, 0.35);
            }
        }
        c.text(&p.title, &fo.title, l.tx, l.ty, l.tw, 22.0, 1.0);
        c.text(&p.artist, &fo.body, l.tx, l.ty + 24.0, l.tw, 18.0, 0.6);
        let (pos, dur) = (p.position(), p.dur);
        let scrubbing = if self.drag_p == i { self.drag } else { None };
        let frac = match scrubbing {
            Some(f) => f as f32,
            None if dur > 0.0 => (pos / dur) as f32,
            None => 0.0,
        };
        let shown = if let Some(f) = scrubbing { f * dur } else { pos };
        c.text(&format!("{}{}", if p.playing { "Now playing \u{B7} " } else { "Paused \u{B7} " }, media::app_name(&p.id)), &fo.small, l.tx, l.ty + 46.0, l.tw, 16.0, 0.4);
        let hot_bar = self.hover == Hover::Scrub(i) || scrubbing.is_some();
        let bh = if hot_bar { 6.0 } else { 4.0 };
        let by = l.sy + 3.0 - bh / 2.0;
        c.rrect(l.tx, by, l.tw, bh, bh / 2.0, 0.16);
        c.rrect_acc(l.tx, by, (l.tw * frac).max(0.0), bh, bh / 2.0, 0.95);
        if hot_bar {
            c.circle_acc(l.tx + l.tw * frac, l.sy + 3.0, 6.0, 1.0);
        }
        c.text(&clock(shown), &fo.small, l.tx, l.sy + 10.0, 60.0, 16.0, 0.55);
        c.text(&clock(dur), &fo.small_r, l.tx + l.tw - 60.0, l.sy + 10.0, 60.0, 16.0, 0.55);
        for (x, kind, h) in [(l.cxs[0], 2, Hover::Prev(i)), (l.cxs[1], if p.playing { 1 } else { 0 }, Hover::Play(i)), (l.cxs[2], 3, Hover::Next(i))] {
            let hot = self.hover == h;
            if kind < 2 {
                let b = if hot { BTN + 4.0 } else { BTN + 2.0 };
                c.rrect_acc(x - b / 2.0, l.cy - b / 2.0, b, b, BTN_R + 1.0, if hot { 1.0 } else { 0.94 });
                c.icon(kind, x, l.cy, 11.0, true, 1.0);
            } else {
                c.rrect(x - BTN / 2.0, l.cy - BTN / 2.0, BTN, BTN, BTN_R, if hot { 0.2 } else { 0.08 });
                c.icon(kind, x, l.cy, 9.0, false, if hot { 1.0 } else { 0.85 });
            }
        }
    }

    /// A paused session as a small card: side by side (top dock) or stacked (side docks).
    unsafe fn tile_card(&self, c: &Ctx, fo: &Fonts, s: &media::Media, i: usize) {
        let (x, y, w, h) = self.tile(i);
        let hot = self.hover == Hover::Sec(i);
        let kind = if s.playing { 1 } else { 0 };
        let status = format!("{} \u{B7} {}", media::app_name(&s.id), if s.playing { "Playing" } else { "Paused" });
        c.rrect(x, y, w, h, 16.0, if hot { 0.12 } else { 0.06 });
        let frac = if s.dur > 0.0 { (s.position() / s.dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        if self.side == Side::Top {
            c.rrect(x + 14.0, y + 14.0, 40.0, 40.0, 10.0, 0.1);
            c.text("\u{E8D6}", &fo.icon_big, x + 14.0, y + 14.0, 40.0, 40.0, 0.75);
            c.text(&media::app_name(&s.id), &fo.hdr, x + 64.0, y + 15.0, w - 78.0, 20.0, 0.85);
            c.text(if s.playing { "Playing" } else { "Paused" }, &fo.small, x + 64.0, y + 35.0, w - 78.0, 16.0, 0.45);
            c.text(&s.title, &fo.title, x + 14.0, y + 70.0, w - 28.0, 22.0, 1.0);
            c.text(&s.artist, &fo.body, x + 14.0, y + 94.0, w - 28.0, 18.0, 0.6);
            let (px, py) = (x + w - 34.0, y + h - 34.0);
            if s.dur > 0.0 {
                let bw = w - 28.0 - 50.0;
                c.rrect(x + 14.0, py - 1.5, bw, 3.0, 1.5, 0.16);
                c.rrect(x + 14.0, py - 1.5, bw * frac, 3.0, 1.5, 0.8);
            }
            c.rrect(px - 18.0, py - 18.0, 36.0, 36.0, BTN_R, if hot { 1.0 } else { 0.85 });
            c.icon(kind, px, py, 8.0, true, 1.0);
        } else {
            c.rrect(x + 12.0, y + 12.0, 48.0, 48.0, 12.0, 0.1);
            c.text("\u{E8D6}", &fo.icon_big, x + 12.0, y + 12.0, 48.0, 48.0, 0.75);
            let tw = w - 72.0 - 56.0;
            c.text(&s.title, &fo.hdr, x + 72.0, y + 10.0, tw, 20.0, 1.0);
            c.text(&s.artist, &fo.body, x + 72.0, y + 29.0, tw, 16.0, 0.6);
            c.text(&status, &fo.small, x + 72.0, y + 46.0, tw, 16.0, 0.4);
            if s.dur > 0.0 {
                c.rrect(x + 16.0, y + h - 6.0, (w - 32.0) * frac, 2.0, 1.0, 0.5);
            }
            let (px, py) = (x + w - 32.0, y + h / 2.0);
            c.rrect(px - 18.0, py - 18.0, 36.0, 36.0, BTN_R, if hot { 1.0 } else { 0.85 });
            c.icon(kind, px, py, 8.0, true, 1.0);
        }
    }

    // ---- input ----------------------------------------------------------------------------

    /// Window client pixels (from lparam) to panel design coordinates.
    fn panel_pt(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as i16 as f32 / self.s;
        let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / self.s;
        let (ox, oy) = self.co();
        (x - ox, y - oy)
    }

    fn hit(&self, x: f32, y: f32) -> Hover {
        let (bw, _) = self.cb();
        if (14.0..42.0).contains(&y) {
            for (tx, tw, tab) in self.tabs() {
                if (tx..tx + tw).contains(&x) {
                    return Hover::Tab(tab);
                }
            }
        }
        if self.tab == Tab::System {
            return Hover::None;
        }
        if self.tab == Tab::Sound {
            let (th, cols, bx) = self.sound_lay();
            for i in 0..self.bri.len() {
                let px = Self::pill_x(bx, i);
                if (px..px + BRI_W).contains(&x) && (SND_Y..SND_Y + th + MUTE_D + 8.0).contains(&y) {
                    return Hover::Bri(i);
                }
            }
            let ax = self.col_x(cols);
            if (ax..ax + COL_W).contains(&x) && (SND_Y..SND_Y + th + MUTE_D + 24.0).contains(&y) {
                return Hover::Add;
            }
            for n in 0..cols.min(self.mix.len() + 1) {
                let cx = self.col_x(n);
                if !(cx..cx + COL_W).contains(&x) {
                    continue;
                }
                if (SND_Y..SND_Y + th).contains(&y) {
                    return Hover::Vol(n);
                }
                if (SND_Y + th + 6.0..SND_Y + th + 6.0 + MUTE_D).contains(&y) {
                    return Hover::Mute(n);
                }
            }
            return Hover::None;
        }
        if self.picking() {
            if (PAD..PAD + 32.0).contains(&x) && (56.0..88.0).contains(&y) {
                return Hover::Back;
            }
            if (PAD..bw - PAD).contains(&x) && y >= PICK_Y {
                let k = ((y - PICK_Y) / PICK_H) as usize;
                if k < self.pick_rows() && self.pick_scroll + k < self.pick_hits.len() {
                    return Hover::App(self.pick_scroll + k);
                }
            }
            return Hover::None;
        }
        if self.tab == Tab::Shelf {
            let n = self.items.notes.len().min(self.note_cap());
            for i in 0..=n {
                let (nx, ny, nw, nh) = self.note_rect(i);
                if (nx..nx + nw).contains(&x) && (ny..ny + nh).contains(&y) {
                    return if i == 0 { Hover::Add } else { Hover::Item(i - 1) };
                }
            }
            return Hover::None;
        }
        if self.tab == Tab::Launch {
            let n = self.shown();
            for i in 0..=n {
                let (tx, ty) = self.tile_xy(i);
                if (tx..tx + TILE).contains(&x) && (ty..ty + TILE).contains(&y) {
                    return if i == n { Hover::Add } else { Hover::Item(i) };
                }
            }
            return Hover::None;
        }
        if self.tab == Tab::Clip {
            if (CHIP_Y..CHIP_Y + CHIP_H).contains(&y) {
                if (bw - PAD - 46.0..bw - PAD).contains(&x) {
                    return Hover::Clear;
                }
                if self.cfg.clip_tools {
                    let mut cx = PAD;
                    for (id, _, w) in self.chips() {
                        if (cx..cx + w).contains(&x) {
                            return Hover::Chip(id);
                        }
                        cx += w + 4.0;
                    }
                }
            }
            if (PAD..bw - PAD).contains(&x) && y >= LIST_Y {
                let i = ((y - LIST_Y) / ROW_PITCH) as usize;
                let view = self.view();
                if i < self.rows() && (y - LIST_Y) - ROW_PITCH * i as f32 <= ROW_H {
                    if let Some(&idx) = view.get(self.scroll + i) {
                        if self.cfg.clip_tools {
                            let rx = bw - PAD - x; // distance from the row's right edge
                            if (PIN_X - 24.0..PIN_X).contains(&rx) && self.hover_row(idx) {
                                return Hover::Pin(idx);
                            }
                            if (DEL_X - 24.0..DEL_X).contains(&rx) && self.hover_row(idx) {
                                return Hover::Del(idx);
                            }
                        }
                        return Hover::Row(idx);
                    }
                }
            }
            return Hover::None;
        }
        if self.media_len() == 0 {
            return Hover::None; // nothing playing: controls are hidden, so they must not react
        }
        let n = self.media_len();
        for i in 0..self.players.min(n) {
            let l = self.lay_n(i);
            if (l.tx - 6.0..l.tx + l.tw + 6.0).contains(&x) && (l.sy - 10.0..l.sy + 16.0).contains(&y) {
                return Hover::Scrub(i);
            }
            for (cx, h) in [(l.cxs[0], Hover::Prev(i)), (l.cxs[1], Hover::Play(i)), (l.cxs[2], Hover::Next(i))] {
                if (x - cx).abs() <= BTN / 2.0 + 2.0 && (y - l.cy).abs() <= BTN / 2.0 + 2.0 {
                    return h;
                }
            }
        }
        for i in 0..self.tiles.min(n.saturating_sub(self.players)) {
            let (tx, ty, tw, th) = self.tile(i);
            if (tx..tx + tw).contains(&x) && (ty..ty + th).contains(&y) {
                return Hover::Sec(i);
            }
        }
        Hover::None
    }

    /// Buttons only exist on the hovered row, so they are hit-tested only once it is hot.
    fn list(&self) -> &Vec<String> {
        &self.items.launch
    }

    /// App picker is up (Launch "+" or Sound "+").
    fn picking(&self) -> bool {
        self.picker && ((self.tab == Tab::Launch && !self.pick_mix) || (self.tab == Tab::Sound && self.pick_mix))
    }

    /// Rebuilds the visible picker rows from the search text (case-insensitive substring).
    fn filter_picker(&mut self) {
        let q = self.pick_q.to_lowercase();
        self.pick_hits = match self.apps.as_ref() {
            Some(a) => a.iter().enumerate().filter(|(_, (n, _))| q.is_empty() || n.to_lowercase().contains(&q)).map(|(i, _)| i).collect(),
            None => Vec::new(),
        };
        self.pick_scroll = 0;
    }

    /// Sticky-note card slot `i` (0 = "+" card): 3 columns, wraps by row.
    fn note_rect(&self, i: usize) -> (f32, f32, f32, f32) {
        let (bw, _) = self.cb();
        let (cols, gap, h) = (3usize, 10.0, 96.0);
        let w = (bw - 2.0 * PAD - gap * (cols - 1) as f32) / cols as f32;
        (PAD + (i % cols) as f32 * (w + gap), 60.0 + (i / cols) as f32 * (h + gap), w, h)
    }

    /// Notes that fit in the box (after the "+" card).
    fn note_cap(&self) -> usize {
        let (_, bh) = self.cb();
        (((bh - 60.0) / 106.0) as usize).max(1) * 3 - 1
    }

    /// Columns and rows of the Shelf/Launch grid for the current box.
    fn grid(&self) -> (usize, usize) {
        let (bw, bh) = self.cb();
        ((((bw - 2.0 * PAD + 8.0) / TILE_P) as usize).max(1), (((bh - 68.0) / TILE_P) as usize).max(1))
    }

    /// Items drawn in the grid; the slot after them is the "+" tile.
    fn shown(&self) -> usize {
        let (cols, rows) = self.grid();
        self.list().len().min(cols * rows - 1)
    }

    /// Native file picker on its own thread (a modal loop here would stall every Lumen module).
    /// Results come back through PICKED + WM_PICKED.
    fn pick_files(&self) {
        if PICKING.swap(true, Ordering::AcqRel) {
            return; // one dialog at a time
        }
        let hwnd = self.hwnd.0 as isize;
        std::thread::spawn(move || unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let paths = pick_dialog(false, HWND(hwnd as *mut _));
            CoUninitialize();
            if !paths.is_empty() {
                *PICKED.lock().unwrap_or_else(|e| e.into_inner()) = Some((false, paths));
                let _ = PostMessageW(HWND(hwnd as *mut _), WM_PICKED, WPARAM(0), LPARAM(0));
            }
            PICKING.store(false, Ordering::Release);
        });
    }

    /// Visible rows of the Launch app picker.
    fn pick_rows(&self) -> usize {
        let (_, bh) = self.cb();
        (((bh - PICK_Y - 12.0) / PICK_H) as usize).max(1)
    }

    fn tile_xy(&self, i: usize) -> (f32, f32) {
        let (bw, _) = self.cb();
        let (cols, _) = self.grid();
        let x0 = PAD + (bw - 2.0 * PAD - (cols as f32 * TILE_P - 8.0)) / 2.0;
        (x0 + (i % cols) as f32 * TILE_P, 60.0 + (i / cols) as f32 * TILE_P)
    }

    fn item_remove(&mut self, i: usize) {
        let v = if self.tab == Tab::Shelf { &mut self.items.notes } else { &mut self.items.launch };
        if i < v.len() {
            v.remove(i);
            self.items.save();
        }
    }

    unsafe fn sample_sys(&mut self) {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        use windows::Win32::System::SystemInformation::{GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX};
        use windows::Win32::System::Threading::GetSystemTimes;
        self.sys_t = Instant::now();
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        let mut cpu = self.sys.map(|s| s.cpu).unwrap_or(0.0);
        let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)).is_ok() {
            // Kernel time includes idle time.
            let (idle, total) = (ft(i), ft(k) + ft(u));
            let (pi, pt) = self.sys_prev;
            if pt > 0 && total > pt {
                cpu = (1.0 - idle.saturating_sub(pi) as f32 / (total - pt) as f32).clamp(0.0, 1.0);
            }
            self.sys_prev = (idle, total);
        }
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        let _ = GlobalMemoryStatusEx(&mut m);
        let mut p = SYSTEM_POWER_STATUS::default();
        let bat = if GetSystemPowerStatus(&mut p).is_ok() && p.BatteryFlag & 128 == 0 && p.BatteryLifePercent <= 100 {
            Some((p.BatteryLifePercent, p.ACLineStatus == 1))
        } else {
            None
        };
        let now_s = GetTickCount64() / 1000;
        // Disk barely moves: hit the volume at most every 30 s.
        let (mut free, mut total) = match self.sys {
            Some(s) if now_s.saturating_sub(s.disk_at) < 30 => (s.disk_free, s.disk_total),
            _ => (0, 0),
        };
        let disk_at = if total == 0 {
            let _ = GetDiskFreeSpaceExW(w!("C:\\"), Some(&mut free), Some(&mut total), None);
            now_s
        } else {
            self.sys.map(|s| s.disk_at).unwrap_or(now_s)
        };
        let used = m.ullTotalPhys.saturating_sub(m.ullAvailPhys);
        self.sys = Some(Sys {
            cpu,
            ram: used as f32 / m.ullTotalPhys.max(1) as f32,
            ram_used: used,
            ram_total: m.ullTotalPhys,
            bat,
            disk_free: free,
            disk_total: total,
            disk_at,
            uptime_s: now_s,
        });
    }

    fn hover_row(&self, idx: usize) -> bool {
        matches!(self.hover, Hover::Row(h) | Hover::Pin(h) | Hover::Del(h) if h == idx)
    }

    fn scrub_frac(&self, i: usize, x: f32) -> f64 {
        let l = self.lay_n(i);
        (((x - l.tx) / l.tw).clamp(0.0, 1.0)) as f64
    }

    fn duration(&self, i: usize) -> f64 {
        self.nth(i).map(|p| p.1).unwrap_or(0.0)
    }

    unsafe fn clip_changed(&mut self) {
        let _ = KillTimer(self.hwnd, T_CLIP);
        if GetClipboardSequenceNumber() == self.own_seq {
            return;
        }
        let mut got = clip::read(self.hwnd);
        for _ in 0..4 {
            if got.is_some() || !clip::has_files() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            got = clip::read(self.hwnd);
        }
        let img = if got.is_none() && self.cfg.clip_images { clip::read_image(self.hwnd) } else { None };
        if got.is_some() || img.is_some() {
            if let Some((files, s)) = got {
                crate::info!("island: clipboard captured (files={files}, {} chars)", s.len());
                self.store.add(files, s);
            } else if let Some(p) = img {
                crate::info!("island: image captured");
                self.store.add_image(p);
            }
            self.store.save();
            if self.mode == Mode::Open {
                self.scroll = 0;
                self.touch();
            } else if self.mode == Mode::Hidden && self.cfg.copy_nudge {
                // Copy nudge: pop the peek pill briefly, then pointer() retracts it.
                self.peek_since = Instant::now();
                self.set_mode(Mode::Peek);
                self.nudge_until = Some(Instant::now() + Duration::from_millis(self.cfg.nudge_ms as u64));
            }
        }
    }

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_FRAME => {
                    if FAST.load(Ordering::Relaxed) {
                        self.tick();
                    }
                }
                WM_TIMER => match wp.0 {
                    T_TICK => {
                        // The vsync pacer drives ticks while animating; the timer is the slow-path fallback.
                        if !FAST.load(Ordering::Relaxed) || self.last_tick.elapsed() > Duration::from_millis(40) {
                            self.tick()
                        }
                    }
                    T_CLIP => self.clip_changed(),
                    _ => return None,
                },
                WM_CLIPBOARDUPDATE => {
                    SetTimer(self.hwnd, T_CLIP, 80, None);
                }
                WM_MEDIA => {
                    if self.mode != Mode::Hidden {
                        self.sync_tiles();
                        self.touch();
                    }
                }
                WM_PICKED => {
                    if let Some((_, paths)) = PICKED.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        self.items.add(paths);
                        self.touch();
                    }
                }
                WM_APPS => {
                    self.apps = APPS.lock().unwrap_or_else(|e| e.into_inner()).take();
                    self.filter_picker();
                    self.touch();
                }
                WM_BRI => {
                    let b = BRI_READ.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
                    let grow = b.len() != self.bri.len();
                    self.bri = b;
                    if grow && self.shown && self.tab == Tab::Sound {
                        self.place();
                    }
                    self.touch();
                }
                WM_MIX => {
                    if let Some((apps, master)) = MIX.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let master = master.unwrap_or(self.master);
                        // Skip while dragging so the poll never fights the finger; repaint only on change.
                        if self.slide.is_none() && self.wheel.is_none() && (apps != self.mix || master != self.master) {
                            let grow = apps.len() != self.mix.len();
                            self.mix = apps;
                            self.master = master;
                            if grow && self.shown && self.tab == Tab::Sound {
                                self.place(); // width follows the app count
                            }
                            self.touch();
                        }
                    }
                }
                WM_CHAR if self.mode == Mode::Open && (self.picking() || self.edit.is_some()) => {
                    let Some(ch) = char::from_u32(wp.0 as u32) else { return Some(LRESULT(0)) };
                    if self.picking() {
                        match ch {
                            '\u{8}' => { self.pick_q.pop(); }
                            '\u{1b}' => self.picker = false,
                            c if !c.is_control() => self.pick_q.push(c),
                            _ => {}
                        }
                        self.filter_picker();
                    } else if let Some(i) = self.edit {
                        if let Some(t) = self.items.notes.get_mut(i) {
                            match ch {
                                '\u{8}' => { t.pop(); }
                                '\u{1b}' => self.edit = None,
                                '\r' => t.push('\n'),
                                c if !c.is_control() => t.push(c),
                                _ => {}
                            }
                        }
                        self.items.save();
                    }
                    self.hover = Hover::None;
                    self.touch();
                    return Some(LRESULT(0));
                }
                WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
                WM_DROPFILES => {
                    let h = HDROP(wp.0 as _);
                    let n = DragQueryFileW(h, 0xFFFFFFFF, None);
                    let mut paths = Vec::new();
                    for i in 0..n {
                        let mut buf = vec![0u16; DragQueryFileW(h, i, None) as usize + 1];
                        let len = DragQueryFileW(h, i, Some(&mut buf)) as usize;
                        paths.push(String::from_utf16_lossy(&buf[..len]));
                    }
                    DragFinish(h);
                    if !paths.is_empty() && self.cfg.tab_launch {
                        self.items.add(paths);
                        self.touch();
                    }
                }
                WM_MOUSEMOVE => {
                    let (x, y) = self.panel_pt(lp);
                    if self.drag.is_some() {
                        self.drag = Some(self.scrub_frac(self.drag_p, x));
                        self.touch();
                    }
                    if let Some(n) = self.slide {
                        self.slide_to(n, y);
                        self.touch();
                        return Some(LRESULT(0));
                    }
                    if let Some((px, py, i)) = self.press {
                        let mut pt = POINT::default();
                        let _ = GetCursorPos(&mut pt);
                        if (pt.x - px).abs() > GetSystemMetrics(SM_CXDRAG) || (pt.y - py).abs() > GetSystemMetrics(SM_CYDRAG) {
                            self.press = None;
                            let _ = ReleaseCapture();
                            // Button already up (fast flick): the release is a click, not a drag.
                            let down = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0;
                            if self.tab != Tab::Clip {
                                if let Some(p) = self.list().get(i).cloned() {
                                    if down {
                                        clip::drag(&clip::Entry { t: 0, files: true, s: p, size: 0, pin: false, img: false });
                                    } else {
                                        open_path(&p);
                                    }
                                }
                            } else if let Some(e) = self.store.items.get(i).cloned() {
                                if down {
                                    clip::drag(&e);
                                } else if clip::write(self.hwnd, &e) {
                                    self.own_seq = GetClipboardSequenceNumber();
                                    self.flash = Some((i, Instant::now()));
                                    if self.cfg.paste_on_click { send_paste(); }
                                }
                            }
                            self.hover = Hover::None;
                            self.touch();
                            return Some(LRESULT(0));
                        }
                    }
                    let h = if self.mode == Mode::Open { self.hit(x, y) } else { Hover::None };
                    if h != self.hover {
                        self.hover = h;
                        self.touch();
                    }
                }
                WM_LBUTTONDOWN => {
                    if self.mode != Mode::Open {
                        return Some(LRESULT(0));
                    }
                    let (x, y) = self.panel_pt(lp);
                    match self.hit(x, y) {
                        Hover::Scrub(i) if self.duration(i) > 0.0 => {
                            self.drag_p = i;
                            self.drag = Some(self.scrub_frac(i, x));
                            self.seek = self.nth(i);
                            SetCapture(self.hwnd);
                        }
                        Hover::Tab(t) => {
                            if self.tab != t {
                                let tabs = self.tabs();
                                let pos = |x: Tab| tabs.iter().position(|q| q.2 == x).unwrap_or(0);
                                self.tab_dir = if pos(t) > pos(self.tab) { 1.0 } else { -1.0 };
                                self.tab_a = 0.0;
                            }
                            self.tab = t;
                            self.picker = false;
                            self.edit = None;
                            self.scroll_f = 0.0;
                            self.hover = Hover::None;
                            if t == Tab::Sound {
                                self.sample_mix();
                                read_brightness(self.hwnd);
                            }
                            if self.shown {
                                self.place();
                            }
                        }
                        Hover::Prev(i) => {
                            if let Some((id, _)) = self.nth(i) {
                                self.send(media::Cmd::Prev(id));
                            }
                        }
                        Hover::Play(i) => {
                            if let Some((id, _)) = self.nth(i) {
                                self.send(media::Cmd::Toggle(id));
                            }
                        }
                        Hover::Next(i) => {
                            if let Some((id, _)) = self.nth(i) {
                                self.send(media::Cmd::Next(id));
                            }
                        }
                        Hover::Sec(i) => {
                            if let Some(id) = self.sec_id(i) {
                                self.send(media::Cmd::Toggle(id));
                            }
                        }
                        Hover::Clear => {
                            self.store.clear();
                            self.store.save();
                            self.flash = None;
                            self.scroll = 0;
                            self.scroll_f = 0.0;
                        }
                        Hover::Chip(f) => {
                            self.filter = f;
                            self.scroll = 0;
                            self.scroll_f = 0.0;
                        }
                        Hover::Pin(i) => {
                            self.store.toggle_pin(i);
                            self.store.save();
                        }
                        Hover::Del(i) => {
                            self.store.remove(i);
                            self.store.save();
                            self.flash = None;
                            self.hover = Hover::None;
                            self.scroll = self.scroll.min(self.view().len().saturating_sub(self.rows()));
                        }
                        Hover::Item(i) if self.tab == Tab::Shelf => {
                            self.edit = if self.edit == Some(i) { None } else { Some(i) };
                            let _ = SetForegroundWindow(self.hwnd);
                        }
                        Hover::Row(i) | Hover::Item(i) => {
                            // Copy/open happens on release (a click); moving first turns it into a drag-out.
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            self.press = Some((pt.x, pt.y, i));
                            SetCapture(self.hwnd);
                        }
                        Hover::Add if self.tab == Tab::Launch || self.tab == Tab::Sound => {
                            let mix = self.tab == Tab::Sound;
                            // Both pickers share `apps`; rescan when switching kind (sessions change anyway).
                            if mix || self.pick_mix || self.apps.is_none() {
                                self.apps = None;
                                if mix { scan_sessions(self.hwnd) } else { scan_apps(self.hwnd) }
                            }
                            self.picker = true;
                            self.pick_mix = mix;
                            self.pick_q.clear();
                            self.filter_picker();
                            self.hover = Hover::None;
                            let _ = SetForegroundWindow(self.hwnd); // so typing reaches the search box
                        }
                        Hover::Add if self.tab == Tab::Shelf => {
                            self.items.notes.push(String::new());
                            self.edit = Some(self.items.notes.len() - 1);
                            self.items.save();
                            let _ = SetForegroundWindow(self.hwnd);
                        }
                        Hover::Add => self.pick_files(),
                        Hover::Back => {
                            self.picker = false;
                            self.hover = Hover::None;
                        }
                        Hover::App(k) => {
                            if let Some((_, p)) = self.pick_hits.get(k).and_then(|&a| self.apps.as_ref()?.get(a)).cloned() {
                                if self.pick_mix {
                                    if !self.items.mix_pins.iter().any(|q| q.eq_ignore_ascii_case(&p)) {
                                        self.items.mix_pins.push(p);
                                        self.items.save();
                                    }
                                    self.sample_mix();
                                } else {
                                    self.items.add(vec![p]);
                                }
                            }
                            self.picker = false;
                            self.hover = Hover::None;
                        }
                        Hover::Vol(n) => {
                            self.slide = Some(n);
                            SetCapture(self.hwnd);
                            self.slide_to(n, y);
                        }
                        Hover::Bri(i) if matches!(self.bri.get(i), Some(Some(_))) => {
                            self.slide = Some(usize::MAX - i);
                            SetCapture(self.hwnd);
                            self.slide_to(usize::MAX - i, y);
                        }
                        Hover::Mute(n) => self.toggle_mute(n),
                        _ => {}
                    }
                    self.touch();
                }
                WM_LBUTTONUP => {
                    if self.slide.take().is_some() {
                        let _ = ReleaseCapture();
                        self.touch();
                    }
                    if let Some(f) = self.drag.take() {
                        let _ = ReleaseCapture();
                        if let Some((id, d)) = self.seek.take() {
                            self.send(media::Cmd::Seek(id, f * d));
                        }
                        self.touch();
                    }
                    if let Some((_, _, i)) = self.press.take() {
                        let _ = ReleaseCapture();
                        if self.tab != Tab::Clip {
                            if let Some(p) = self.list().get(i).cloned() {
                                open_path(&p);
                            }
                        } else if let Some(e) = self.store.items.get(i).cloned() {
                            if clip::write(self.hwnd, &e) {
                                self.own_seq = GetClipboardSequenceNumber();
                                self.flash = Some((i, Instant::now()));
                                if self.cfg.paste_on_click { send_paste(); }
                            }
                        }
                        self.touch();
                    }
                }
                WM_CAPTURECHANGED => {
                    if self.drag.take().is_some() | self.press.take().is_some() | self.slide.take().is_some() {
                        self.touch();
                    }
                }
                WM_RBUTTONUP => {
                    if self.mode == Mode::Open {
                        let (x, y) = self.panel_pt(lp);
                        let h = self.hit(x, y);
                        if let (Tab::Sound, Hover::Vol(n) | Hover::Mute(n)) = (self.tab, h) {
                            // Right-click an app column: unpin it (it still shows while playing).
                            if let Some(key) = n.checked_sub(1).and_then(|k| self.mix.get(k)).map(|c| c.key.clone()) {
                                self.items.mix_pins.retain(|q| !q.eq_ignore_ascii_case(&key));
                                self.items.save();
                                self.sample_mix();
                            }
                        } else if let Hover::Item(i) = h {
                            if self.tab == Tab::Shelf {
                                self.edit = match self.edit {
                                    Some(e) if e == i => None,
                                    Some(e) if e > i => Some(e - 1),
                                    e => e,
                                };
                            }
                            self.item_remove(i);
                            self.hover = Hover::None;
                            self.touch();
                        } else if let Hover::Row(i) | Hover::Pin(i) | Hover::Del(i) = self.hit(x, y) {
                            self.store.remove(i);
                            self.flash = None;
                            self.store.save();
                            self.scroll = self.scroll.min(self.view().len().saturating_sub(self.rows()));
                            self.hover = Hover::None;
                            self.touch();
                        }
                    }
                }
                WM_MOUSEWHEEL | 0x020E => {
                    let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
                    // Sound tab: wheel (up or right = louder) eases the hovered fader toward a target.
                    if self.tab == Tab::Sound && !self.picking() {
                        let id = match self.hover {
                            Hover::Vol(n) | Hover::Mute(n) => Some(n),
                            Hover::Bri(i) => Some(usize::MAX - i),
                            _ => None,
                        };
                        if let Some(id) = id {
                            if let Some(cur) = self.level_of(id) {
                                let base = match self.wheel { Some((w, t, _)) if w == id => t, _ => cur };
                                let t = (base + delta as f32 / 120.0 * 0.04).clamp(0.0, 1.0);
                                let c = match self.wheel { Some((w, _, c)) if w == id => c, _ => cur };
                                self.wheel = Some((id, t, c));
                                self.touch();
                            }
                        }
                        return Some(LRESULT(0));
                    }
                    if msg != WM_MOUSEWHEEL {
                        return Some(LRESULT(0));
                    }
                    // Precision touchpads send deltas under 120; still move one step.
                    let notches = if delta.abs() >= 120 { delta / 120 } else { delta.signum() };
                    if self.tab == Tab::Media && self.cfg.media_controls {
                        // MK_SHIFT: brightness, plain wheel: volume; both eased like the Sound faders.
                        let bright = wp.0 & 0x4 != 0;
                        let id = if bright { usize::MAX - mixer::monitor_of(self.hwnd) } else { 0 };
                        let live = matches!(self.wheel, Some((w, _, _)) if w == id);
                        if !live {
                            // Levels are only sampled on the Sound tab; read the device fresh.
                            if bright {
                                let i = usize::MAX - id;
                                if self.bri.len() <= i { self.bri.resize(i + 1, None); }
                                self.bri[i] = mixer::brightness(i, None);
                            } else if let Some(m) = mixer::master() {
                                self.master = m;
                            }
                        }
                        if let Some(cur) = self.level_of(id) {
                            let (base, c) = match self.wheel { Some((w, t, c)) if w == id => (t, c), _ => (cur, cur) };
                            let t = (base + notches as f32 * if bright { 0.1 } else { 0.04 }).clamp(0.0, 1.0);
                            self.wheel = Some((id, t, c));
                            self.hud = Some((bright, c, Instant::now()));
                            self.touch();
                        }
                    }
                    if self.picking() {
                        let max = self.pick_hits.len().saturating_sub(self.pick_rows()) as i32;
                        let n = (self.pick_scroll as i32 - notches * 3).clamp(0, max) as usize;
                        if n != self.pick_scroll {
                            self.pick_scroll = n;
                            self.hover = Hover::None; // re-hit on next mouse move
                            self.touch();
                        }
                        return Some(LRESULT(0));
                    }
                    if self.tab != Tab::Clip {
                        return Some(LRESULT(0));
                    }
                    let max =self.view().len().saturating_sub(self.rows()) as i32;
                    let n = (self.scroll as i32 - notches * 2).clamp(0, max) as usize;
                    if n != self.scroll {
                        self.scroll = n;
                        self.touch();
                    }
                }
                _ => return None,
            }
        }
        Some(LRESULT(0))
    }

    fn send(&self, c: media::Cmd) {
        if let Some(w) = &self.worker {
            w.send(c);
        }
    }
}

impl Drop for Island {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, T_TICK);
            let _ = KillTimer(self.hwnd, T_CLIP);
            let _ = RemoveClipboardFormatListener(self.hwnd);
            self.store.save();
            self.worker = None;
            let _ = DestroyWindow(self.hwnd);
            self.release_surface();
            let _ = DeleteDC(self.dc);
        }
    }
}

/// A 256x256 warm-to-violet sweep, as a 24-bit BMP (WIC decodes it like any cover). Used only by `--island-snapshot media`.
fn sample_cover() -> Vec<u8> {
    const N: usize = 256;
    let mut b = Vec::with_capacity(54 + N * N * 3);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&((54 + N * N * 3) as u32).to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&(N as i32).to_le_bytes());
    b.extend_from_slice(&(N as i32).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0; 24]);
    // Original sunset-over-water artwork (rows are bottom-up in a BMP).
    let mix = |a: [f32; 3], c: [f32; 3], t: f32| [a[0] + (c[0] - a[0]) * t, a[1] + (c[1] - a[1]) * t, a[2] + (c[2] - a[2]) * t];
    for y in 0..N {
        let yy = (N - 1 - y) as f32;
        for x in 0..N {
            let xf = x as f32;
            let hor = 168.0;
            let mut c = if yy < hor { mix([0.07, 0.05, 0.22], [0.95, 0.38, 0.42], (yy / hor).powf(1.6)) } else { mix([0.10, 0.06, 0.24], [0.03, 0.03, 0.12], ((yy - hor) / (N as f32 - hor)).min(1.0)) };
            let (dx, dy) = (xf - 128.0, yy - 140.0);
            let d = (dx * dx + dy * dy).sqrt();
            let slit = yy > 128.0 && ((yy - 128.0) as i32 % 9) < ((yy - 128.0) / 9.0) as i32;
            if d < 62.0 && yy < hor && !slit {
                c = mix([1.0, 0.82, 0.40], [0.98, 0.35, 0.55], ((yy - 78.0) / 90.0).clamp(0.0, 1.0));
            }
            if yy >= hor && dx.abs() < 46.0 - (yy - hor) * 0.5 && ((yy as i32) % 6) < 3 {
                c = mix(c, [0.98, 0.45, 0.50], 0.55);
            }
            b.extend_from_slice(&[(c[2] * 255.0) as u8, (c[1] * 255.0) as u8, (c[0] * 255.0) as u8]);
        }
    }
    b
}

unsafe fn shell_icon(rt: &ID2D1DCRenderTarget, wic: &IWICImagingFactory, path: &str) -> Option<ID2D1Bitmap> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW::default();
    let r = SHGetFileInfoW(PCWSTR(wide.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&mut info as *mut SHFILEINFOW), std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_LARGEICON);
    if r == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let out = (|| {
        let b = wic.CreateBitmapFromHICON(info.hIcon).ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(&b, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || w > 256 || h > 256 {
            return None;
        }
        let mut data = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        rt.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(data.as_ptr() as *const _), w * 4, &props).ok()
    })();
    let _ = DestroyIcon(info.hIcon);
    out
}

unsafe fn decode(rt: &ID2D1DCRenderTarget, wic: &IWICImagingFactory, bytes: &[u8]) -> Option<ID2D1Bitmap> {
    let stream = wic.CreateStream().ok()?;
    stream.InitializeFromMemory(bytes).ok()?;
    let dec = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
    let frame = dec.GetFrame(0).ok()?;
    let conv = wic.CreateFormatConverter().ok()?;
    conv.Initialize(&frame, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    let (mut w, mut h) = (0u32, 0u32);
    conv.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let mut data = vec![0u8; (w * h * 4) as usize];
    conv.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    rt.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(data.as_ptr() as *const _), w * 4, &props).ok()
}

/// The cover shrunk to 48x48 and blurred smooth: its own colours where they sit, like ambient light.
unsafe fn cover_blur(wic: &IWICImagingFactory, bytes: &[u8]) -> Option<Vec<u8>> {
    let stream = wic.CreateStream().ok()?;
    stream.InitializeFromMemory(bytes).ok()?;
    let frame = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?.GetFrame(0).ok()?;
    let sc = wic.CreateBitmapScaler().ok()?;
    sc.Initialize(&frame, 48, 48, WICBitmapInterpolationModeFant).ok()?;
    let conv = wic.CreateFormatConverter().ok()?;
    conv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    let mut px = vec![0u8; 48 * 48 * 4];
    conv.CopyPixels(std::ptr::null(), 48 * 4, &mut px).ok()?;
    blur_sat(&mut px, 48, 48, 1.5);
    blur_sat(&mut px, 48, 48, 1.0);
    // Auto-levels: a dark cover (black with a few orange bits) would average to black, so stretch the
    // brightest blurred area to a fixed level. The panel's dark layer then keeps text readable.
    let top = px.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])).max().unwrap_or(0) as f32 / 255.0;
    let gain = (0.6 / top.max(0.05)).clamp(1.0, 10.0);
    for p in px.chunks_exact_mut(4) {
        for c in &mut p[..3] {
            *c = (*c as f32 * gain).min(255.0) as u8;
        }
    }
    Some(px)
}

/// The cover's palette as 5 rgb triples: dark glass base, three glows, light accent.
/// Lightness is fixed per role so white text stays readable on the result.
unsafe fn cover_palette(wic: &IWICImagingFactory, bytes: &[u8]) -> Option<[f32; 15]> {
    let stream = wic.CreateStream().ok()?;
    stream.InitializeFromMemory(bytes).ok()?;
    let frame = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?.GetFrame(0).ok()?;
    let sc = wic.CreateBitmapScaler().ok()?;
    sc.Initialize(&frame, 16, 16, WICBitmapInterpolationModeFant).ok()?;
    let conv = wic.CreateFormatConverter().ok()?;
    conv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    let mut d = [0u8; 16 * 16 * 4];
    conv.CopyPixels(std::ptr::null(), 64, &mut d).ok()?;
    // 12 hue buckets weighted by vividness (saturation x brightness): the cover's colours, ranked.
    let mut bins = [(0.0f32, [0.0f32; 3]); 12];
    for p in d.chunks_exact(4) {
        let (r, g, b) = (p[2] as f32 / 255.0, p[1] as f32 / 255.0, p[0] as f32 / 255.0);
        let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
        let c = mx - mn;
        if c < 0.08 {
            continue;
        }
        let w = c * mx;
        let h = if mx == r { ((g - b) / c).rem_euclid(6.0) } else if mx == g { (b - r) / c + 2.0 } else { (r - g) / c + 4.0 };
        let e = &mut bins[(h * 2.0) as usize % 12];
        e.0 += w;
        e.1[0] += w * h * 60.0;
        e.1[1] += w * (c / mx);
        e.1[2] += w;
    }
    // (hue, saturation, weight) per bucket, best first.
    let mut v: Vec<(f32, f32, f32)> = bins.iter().filter(|b| b.0 > 0.0).map(|b| (b.1[0] / b.0, b.1[1] / b.0, b.0)).collect();
    v.sort_by(|a, b| b.2.total_cmp(&a.2));
    // Greyscale covers: neutral glass, soft white accent.
    let Some(&(h1, s1, w1)) = v.first().filter(|f| f.2 > 0.6) else {
        let g = hsl(0.0, 0.0, 0.26);
        let mut p = [0.0f32; 15];
        for (i, c) in [hsl(0.0, 0.0, 0.08), g, g, g, (0.92, 0.92, 0.94)].iter().enumerate() {
            p[i * 3..i * 3 + 3].copy_from_slice(&[c.0, c.1, c.2]);
        }
        return Some(p);
    };
    let far = |h: f32, o: f32| {
        let d = (h - o).rem_euclid(360.0);
        d.min(360.0 - d)
    };
    // Second and third colours: next-best hues at least 50 degrees apart, else a shifted copy of the first.
    let (h2, s2) = v.iter().find(|x| far(x.0, h1) >= 50.0 && x.2 > w1 * 0.15).map(|x| (x.0, x.1)).unwrap_or((h1 + 40.0, s1));
    let (h3, s3) = v.iter().find(|x| far(x.0, h1) >= 50.0 && far(x.0, h2) >= 50.0 && x.2 > w1 * 0.15).map(|x| (x.0, x.1)).unwrap_or((h1 - 35.0, s1));
    let sat = |s: f32| s.clamp(0.5, 0.85);
    let mut p = [0.0f32; 15];
    for (i, c) in [
        hsl(h1, s1.clamp(0.3, 0.6), 0.09),
        hsl(h1, sat(s1), 0.36),
        hsl(h2, sat(s2), 0.34),
        hsl(h3, sat(s3), 0.32),
        hsl(h1, 0.9, 0.62),
    ]
    .iter()
    .enumerate()
    {
        p[i * 3..i * 3 + 3].copy_from_slice(&[c.0, c.1, c.2]);
    }
    Some(p)
}

/// A saved clipboard image shrunk to fit 64x64, so the cache holds a few KB, not a full screenshot.
unsafe fn thumb(rt: &ID2D1DCRenderTarget, wic: &IWICImagingFactory, path: &str) -> Option<ID2D1Bitmap> {
    let bytes = std::fs::read(path).ok()?;
    let stream = wic.CreateStream().ok()?;
    stream.InitializeFromMemory(&bytes).ok()?;
    let dec = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
    let frame = dec.GetFrame(0).ok()?;
    let (mut w, mut h) = (0u32, 0u32);
    frame.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let k = (64.0 / w.max(h) as f32).min(1.0);
    let (tw, th) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
    let sc = wic.CreateBitmapScaler().ok()?;
    sc.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant).ok()?;
    let conv = wic.CreateFormatConverter().ok()?;
    conv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    let mut data = vec![0u8; (tw * th * 4) as usize];
    conv.CopyPixels(std::ptr::null(), tw * 4, &mut data).ok()?;
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    rt.CreateBitmap(D2D_SIZE_U { width: tw, height: th }, Some(data.as_ptr() as *const _), tw * 4, &props).ok()
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: w!("LumenIsland"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        w!("LumenIsland"),
        w!("Lumen Island"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        instance,
        None,
    )?;
    // Vsync pacer: while animating, wake the UI thread once per display refresh (SetTimer is capped near 60 Hz).
    let h = hwnd.0 as isize;
    std::thread::spawn(move || loop {
        if FAST.load(Ordering::Relaxed) {
            let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            if !FRAME_PENDING.swap(true, Ordering::AcqRel) && PostMessageW(HWND(h as *mut _), WM_FRAME, WPARAM(0), LPARAM(0)).is_err() {
                FRAME_PENDING.store(false, Ordering::Release);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    });
    Ok(hwnd)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_GETMINMAXINFO | WM_SIZE | WM_MOVE | WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED => return DefWindowProcW(hwnd, msg, wp, lp),
        // Re-arm the pacer here, not in the handler: a frame that arrives while the app is borrowed (re-entrant
        // message) is dropped by island_message, and a flag left set would stall the pacer until restart.
        WM_FRAME => FRAME_PENDING.store(false, Ordering::Release),
        _ => {}
    }
    if let Some(r) = crate::app::island_message(msg, wp, lp) {
        return r;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Runs the Open dialog (called on the picker thread, COM already initialised).
/// Launch filters to apps/shortcuts and keeps .lnk files as-is; Shelf takes anything.
unsafe fn pick_dialog(shelf: bool, owner: HWND) -> Vec<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_NODEREFERENCELINKS, SIGDN_FILESYSPATH};
    let mut paths = Vec::new();
    let Ok(d) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else { return paths };
    let mut o = FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM;
    if !shelf {
        o |= FOS_NODEREFERENCELINKS;
        let f = [
            COMDLG_FILTERSPEC { pszName: w!("Apps and shortcuts"), pszSpec: w!("*.exe;*.lnk;*.url;*.bat;*.cmd") },
            COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
        ];
        let _ = d.SetFileTypes(&f);
        let _ = d.SetTitle(w!("Add to Launch"));
    } else {
        let _ = d.SetTitle(w!("Add to Shelf"));
    }
    let _ = d.SetOptions(o);
    if d.Show(owner).is_err() {
        return paths; // cancelled
    }
    let Ok(items) = d.GetResults() else { return paths };
    for i in 0..items.GetCount().unwrap_or(0) {
        if let Ok(p) = items.GetItemAt(i).and_then(|it| it.GetDisplayName(SIGDN_FILESYSPATH)) {
            if let Ok(s) = p.to_string() {
                paths.push(s);
            }
            CoTaskMemFree(Some(p.0 as _));
        }
    }
    paths
}

/// Mean luminance of the BGRA capture (`d` screen px per capture px) over the window rect at `org` (monitor px).
fn backdrop_lum(px: &[u8], sw: i32, sh: i32, d: f32, org: (i32, i32), size: (i32, i32)) -> f32 {
    let x0 = ((org.0 as f32 / d) as i32).clamp(0, sw - 1);
    let y0 = ((org.1 as f32 / d) as i32).clamp(0, sh - 1);
    let x1 = (((org.0 + size.0) as f32 / d).ceil() as i32).clamp(x0 + 1, sw);
    let y1 = (((org.1 + size.1) as f32 / d).ceil() as i32).clamp(y0 + 1, sh);
    let (mut sum, mut n) = (0.0f32, 0u32);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * sw + x) * 4) as usize;
            sum += 0.114 * px[i] as f32 + 0.587 * px[i + 1] as f32 + 0.299 * px[i + 2] as f32;
            n += 1;
        }
    }
    sum / n.max(1) as f32 / 255.0
}
