//! Island: an edge-docked, hidden-by-default pill that grows into a panel with the clipboard
//! history and a media player. One layered window, redrawn only while it moves or changes.
//! The module is created and dropped by the app, so it costs nothing while turned off.

mod clip;
mod media;

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
fn send_paste() {
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
        },
    };
    let seq = [key(VK_CONTROL, false), key(VK_V, false), key(VK_V, true), key(VK_CONTROL, true)];
    unsafe { SendInput(&seq, std::mem::size_of::<INPUT>() as i32) };
}
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::IslandConfig;

const WM_MEDIA: u32 = WM_APP + 1;
const T_TICK: usize = 1;
const T_CLIP: usize = 2;

// Design units (scaled by DPI x the user's scale).
// Content box: wide and short on the top edge, narrow and tall on the sides. Same for both tabs.
const TOP_BOX: (f32, f32) = (900.0, 270.0);
const SIDE_BOX: (f32, f32) = (360.0, 600.0);
const F: f32 = 14.0; // fillet margin along the docked edge
const PEEK: f32 = 0.28;
const PEEK_EXT: f32 = 24.0;
const PEEK_HH: f32 = 64.0;
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
const TAB_GAP: f32 = 4.0;
const TAB_R: f32 = 16.0;
const PRESS: f32 = 0.95;
const LIFT: f32 = 1.5;
const TABS: [(f32, f32, Tab); 2] = [(PAD, 96.0, Tab::Clip), (PAD + 96.0 + TAB_GAP, 76.0, Tab::Media)];

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Clip,
    Media,
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
}

#[derive(Clone)]
struct Fonts {
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_r: IDWriteTextFormat,
    center: IDWriteTextFormat,
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
    unsafe fn new(dw: &IDWriteFactory) -> Result<Fonts> {
        let ui = w!("Segoe UI");
        let ic = w!("Segoe MDL2 Assets");
        let l = DWRITE_TEXT_ALIGNMENT_LEADING;
        let c = DWRITE_TEXT_ALIGNMENT_CENTER;
        Ok(Fonts {
            title: mk(dw, ui, 15.0, 600, l)?,
            body: mk(dw, ui, 13.0, 400, l)?,
            small: mk(dw, ui, 11.0, 400, l)?,
            small_r: mk(dw, ui, 11.0, 400, DWRITE_TEXT_ALIGNMENT_TRAILING)?,
            center: mk(dw, ui, 12.0, 400, c)?,
            hdr: mk(dw, ui, 12.0, 600, l)?,
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
    if e.files {
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

/// Drawing helper: panel design coordinates, one reusable brush.
struct Ctx<'a> {
    rt: &'a ID2D1DCRenderTarget,
    br: &'a ID2D1SolidColorBrush,
    fg: (f32, f32, f32),
    alpha: f32,
    ox: f32,
    oy: f32,
}

impl Ctx<'_> {
    unsafe fn col(&self, a: f32) {
        self.br.SetColor(&color(self.fg.0, self.fg.1, self.fg.2, a * self.alpha));
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
    /// Cover art per session id with the generation it was decoded from; dropped with the render target.
    arts: std::collections::HashMap<String, (u32, Option<ID2D1Bitmap>)>,
    /// Shell icons by path; dropped whenever the render target is recreated.
    icons: std::collections::HashMap<String, Option<ID2D1Bitmap>>,
    tab: Tab,
    /// Playing sessions that get a full player (1..=MAX_PLAYERS), and paused ones shown as tiles (0..=MAX_TILES).
    players: usize,
    tiles: usize,
    /// Player being scrubbed.
    drag_p: usize,
    /// Per session id: eased (x, y, alpha) of its card, for layout animation.
    anim: std::collections::HashMap<String, (f32, f32, f32)>,
    anim_t: Instant,
    animating: bool,
    /// Row hover lift / press squash progress (0..1) for row `row_i`.
    row_t: f32,
    press_t: f32,
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
    flash: Option<(usize, Instant)>,
    store: clip::Store,
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
                arts: Default::default(),
                icons: Default::default(),
                tab: Tab::Clip,
                players: 1,
                tiles: 0,
                drag_p: 0,
                anim: Default::default(),
                anim_t: now,
                animating: false,
                row_t: 0.0,
                press_t: 0.0,
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
                hid_at: None,
                peek_since: now,
                hover: Hover::None,
                drag: None,
                seek: None,
                press: None,
                scroll: 0,
                flash: None,
                store: clip::Store::load(cfg.history_days),
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

    pub fn set_config(&mut self, cfg: &IslandConfig) {
        self.cfg = cfg.clone();
        self.side = parse_side(&cfg.side);
        self.rgb = parse_rgb(&cfg.bg);
        self.store.set_days(cfg.history_days);
        if self.shown {
            unsafe { self.place() };
            self.touch();
        }
    }

    // ---- geometry -------------------------------------------------------------------------

    /// Content box (w, h): wide on the top edge, tall on the sides. Same for both tabs.
    fn cb(&self) -> (f32, f32) {
        let (np, t) = if self.tab == Tab::Media { (self.players.max(1), self.tiles as f32) } else { (1, 0.0) };
        if self.side == Side::Top {
            (self.player_w() * np as f32 + t * (TILE_W + TILE_GAP), TOP_BOX.1)
        } else if t > 0.0 {
            (SIDE_BOX.0, SIDE_BOX.1.max(self.tiles_y() + t * (TILE_H + TILE_GAP) - TILE_GAP + 20.0))
        } else if np > 1 {
            (SIDE_BOX.0, SIDE_BOX.1.max(MULTI_Y + np as f32 * MULTI_H + 20.0))
        } else {
            SIDE_BOX
        }
    }

    /// Width of one player on the top dock: the full box alone, narrower when they share.
    fn player_w(&self) -> f32 {
        if self.players.max(1) == 1 { TOP_BOX.0 } else { MULTI_W }
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

    /// Resizes the panel when the number of players or tiles it should show changes.
    unsafe fn sync_tiles(&mut self) {
        let (p, t) = Self::counts(&self.ordered());
        let p = p.max(1);
        if t != self.tiles || p != self.players {
            self.tiles = t;
            self.players = p;
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

    /// Clipboard rows that fit under the tab bar.
    fn rows(&self) -> usize {
        (((self.cb().1 - 72.0 + ROW_GAP) / ROW_PITCH) as usize).max(1)
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
        if m == Mode::Open {
            self.scroll = 0;
        }
        if self.instant() {
            self.v = self.target();
            self.vel = 0.0;
        }
        self.touch();
    }

    unsafe fn show(&mut self) {
        let (p, t) = Self::counts(&self.ordered());
        self.players = p.max(1);
        self.tiles = t;
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
        let omega = 6.0 / (self.cfg.animation_ms.max(1) as f32 / 1000.0);
        let zeta = if self.cfg.animation == "liquid" { 0.6 } else { 1.0 };
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
        let moving = a != self.row_t || b != self.press_t;
        self.row_t = a;
        self.press_t = b;
        moving
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        unsafe { self.pointer(now) };
        let moving = self.step(dt);
        self.row_anim = self.step_rows(dt);
        let mut flashing = false;
        if let Some((_, t)) = self.flash {
            if t.elapsed() < Duration::from_millis(900) {
                flashing = true;
            } else {
                self.flash = None;
                self.dirty = true;
            }
        }
        if self.mode == Mode::Open && now.duration_since(self.last_draw) >= Duration::from_millis(400) {
            if let Some(wk) = &self.worker {
                if wk.state.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|x| x.playing) {
                    self.dirty = true;
                }
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
        if self.shown && (moving || self.dirty || flashing || self.animating || self.row_anim) {
            unsafe {
                if self.paint().is_err() {
                    let _ = self.paint();
                }
            }
            self.dirty = false;
            self.last_draw = now;
        }
        self.timer(if moving || self.dirty || flashing || self.animating || self.row_anim { 16 } else if self.mode == Mode::Hidden { 10 } else { 33 });
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
                if (on && depth >= 2) || (at_edge && now.duration_since(self.peek_since) >= Duration::from_millis(30)) {
                    self.set_mode(Mode::Open);
                } else if !on && !at_edge && !near {
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
            self.fonts = Some(Fonts::new(&self.dw)?);
        }
        if self.rt.is_some() && self.surf == self.px {
            return Ok(());
        }
        self.rt = None;
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
        let drawn = self.draw(&rt);
        let ended = rt.EndDraw(None, None);
        if drawn.is_err() || ended.is_err() {
            self.rt = None;
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
        let r = (12.0 + 16.0 * k).min(hh).min((ext - f).max(0.0));
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

    unsafe fn draw(&mut self, rt: &ID2D1DCRenderTarget) -> Result<()> {
        let (ext, hh, k) = self.shape();
        if ext < 0.5 {
            return Ok(());
        }
        let br = rt.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 1.0), None)?;
        let geo = self.geometry(ext, hh, k)?;
        let (r, g, b) = self.rgb;
        br.SetColor(&color(r, g, b, 1.0));
        rt.FillGeometry(&geo, &br, None::<&ID2D1Brush>);
        let light = 0.299 * r + 0.587 * g + 0.114 * b > 0.6;
        let fg = if light { (0.06, 0.06, 0.07) } else { (1.0, 1.0, 1.0) };
        let a = ((k - 0.45) / 0.45).clamp(0.0, 1.0);
        let alpha = a * a * (3.0 - 2.0 * a);
        if alpha < 0.01 {
            return Ok(());
        }
        let (l, t, rr, bb) = self.bounds(ext, hh);
        rt.PushAxisAlignedClip(&rect(l, t, rr - l, bb - t), D2D1_ANTIALIAS_MODE_ALIASED);
        let (ox, oy) = self.co();
        if self.cfg.animation == "zoom" {
            let z = 0.9 + 0.1 * alpha;
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
    unsafe fn icon(&mut self, rt: &ID2D1DCRenderTarget, path: &str) -> Option<ID2D1Bitmap> {
        if let Some(b) = self.icons.get(path) {
            return b.clone();
        }
        if self.icons.len() > 300 {
            self.icons.clear();
        }
        if self.wic.is_none() {
            self.wic = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok();
        }
        let bmp = self.wic.as_ref().and_then(|wic| shell_icon(rt, wic, path));
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
        let c = Ctx { rt, br, fg, alpha, ox, oy };
        let (bw, _) = self.cb();

        // Tab bar.
        for (x, w, tab) in TABS {
            let label = if tab == Tab::Clip { "Clipboard" } else { "Media" };
            let on = self.tab == tab;
            if on {
                c.rrect(x, 14.0, w, 28.0, TAB_R, 0.16);
            } else if self.hover == Hover::Tab(tab) {
                c.rrect(x, 14.0, w, 28.0, TAB_R, 0.08);
            }
            c.text(label, &fo.center, x, 14.0, w, 28.0, if on { 1.0 } else { 0.55 });
            if tab == Tab::Media && sessions.iter().any(|s| s.playing) {
                c.circle(x + w - 9.0, 21.0, 3.0, 0.9);
            }
        }
        // Thin divider under the header.
        c.rrect(PAD, 49.0, bw - 2.0 * PAD, 1.0, 0.5, 0.1);

        if self.tab == Tab::Clip {
            let cw = bw - 2.0 * PAD;
            if self.hover == Hover::Clear {
                c.rrect(bw - PAD - 66.0, 16.0, 66.0, 24.0, 6.0, 0.1);
            }
            c.text("Clear", &fo.center, bw - PAD - 66.0, 16.0, 66.0, 24.0, 0.6);
            let first = self.scroll;
            let icons: Vec<Option<ID2D1Bitmap>> = (0..self.rows())
                .map(|i| {
                    let e = self.store.items.get(first + i)?;
                    if !e.files {
                        return None;
                    }
                    let path = e.s.lines().next()?.to_string();
                    self.icon(rt, &path)
                })
                .collect();
            let items = &self.store.items;
            if items.is_empty() {
                c.text("\u{E8C8}", &fo.icon_big, PAD, 90.0, cw, 36.0, 0.3);
                c.text("Nothing copied yet", &fo.center, PAD, 130.0, cw, 24.0, 0.7);
                c.text("Copy text or files and they show up here", &fo.center, PAD, 156.0, cw, 18.0, 0.4);
            } else {
                c.text(&format!("{} items", items.len()), &fo.small, PAD + 190.0, 14.0, 90.0, 28.0, 0.4);
            }
            for i in 0..self.rows() {
                let idx = self.scroll + i;
                let Some(e) = items.get(idx) else { break };
                let y = 56.0 + ROW_PITCH * i as f32;
                let hot = self.hover == Hover::Row(idx);
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
                    let x = if hot && !flashing { cw - 98.0 - 64.0 } else { cw - 48.0 - 64.0 };
                    c.text(&sz, &fo.small_r, PAD + x, y, 60.0, ROW_H, 0.45);
                }
                if hot && !flashing {
                    c.text("Click to copy", &fo.small_r, PAD + cw - 98.0, y, 90.0, ROW_H, 0.55);
                } else {
                    c.text(&age(e.t), &fo.small_r, PAD + cw - 48.0, y, 40.0, ROW_H, 0.45);
                }
                if lifted {
                    rt.SetTransform(&Matrix3x2::identity());
                }
            }
            return Ok(());
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
            let cc = Ctx { rt, br, fg, alpha: alpha * a, ox: ox + dx, oy: oy + dy };
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
        c.rrect(l.tx, by, (l.tw * frac).max(0.0), bh, bh / 2.0, 0.9);
        if hot_bar {
            c.circle(l.tx + l.tw * frac, l.sy + 3.0, 6.0, 1.0);
        }
        c.text(&clock(shown), &fo.small, l.tx, l.sy + 10.0, 60.0, 16.0, 0.55);
        c.text(&clock(dur), &fo.small_r, l.tx + l.tw - 60.0, l.sy + 10.0, 60.0, 16.0, 0.55);
        for (x, kind, h) in [(l.cxs[0], 2, Hover::Prev(i)), (l.cxs[1], if p.playing { 1 } else { 0 }, Hover::Play(i)), (l.cxs[2], 3, Hover::Next(i))] {
            let hot = self.hover == h;
            if kind < 2 {
                let b = if hot { BTN + 4.0 } else { BTN + 2.0 };
                c.rrect(x - b / 2.0, l.cy - b / 2.0, b, b, BTN_R + 1.0, if hot { 1.0 } else { 0.94 });
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
            for (tx, tw, tab) in TABS {
                if (tx..tx + tw).contains(&x) {
                    return Hover::Tab(tab);
                }
            }
        }
        if self.tab == Tab::Clip {
            if (bw - 90.0..bw - PAD).contains(&x) && (16.0..40.0).contains(&y) {
                return Hover::Clear;
            }
            if (PAD..bw - PAD).contains(&x) && y >= 56.0 {
                let i = ((y - 56.0) / ROW_PITCH) as usize;
                if i < self.rows() && (y - 56.0) - ROW_PITCH * i as f32 <= ROW_H && self.scroll + i < self.store.items.len() {
                    return Hover::Row(self.scroll + i);
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
        if let Some((files, s)) = got {
            crate::info!("island: clipboard captured (files={files}, {} chars)", s.len());
            self.store.add(files, s);
            self.store.save();
            if self.mode == Mode::Open {
                self.scroll = 0;
                self.touch();
            }
        }
    }

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_TIMER => match wp.0 {
                    T_TICK => self.tick(),
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
                WM_MOUSEACTIVATE => return Some(LRESULT(MA_NOACTIVATE as isize)),
                WM_MOUSEMOVE => {
                    let (x, y) = self.panel_pt(lp);
                    if self.drag.is_some() {
                        self.drag = Some(self.scrub_frac(self.drag_p, x));
                        self.touch();
                    }
                    if let Some((px, py, i)) = self.press {
                        let mut pt = POINT::default();
                        let _ = GetCursorPos(&mut pt);
                        if (pt.x - px).abs() > GetSystemMetrics(SM_CXDRAG) || (pt.y - py).abs() > GetSystemMetrics(SM_CYDRAG) {
                            self.press = None;
                            let _ = ReleaseCapture();
                            // Button already up (fast flick): the release is a click, not a drag.
                            let down = GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0;
                            if let Some(e) = self.store.items.get(i).cloned() {
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
                            self.tab = t;
                            self.hover = Hover::None;
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
                        }
                        Hover::Row(i) => {
                            // Copy happens on release (a click); moving first turns it into a drag-out.
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            self.press = Some((pt.x, pt.y, i));
                            SetCapture(self.hwnd);
                        }
                        _ => {}
                    }
                    self.touch();
                }
                WM_LBUTTONUP => {
                    if let Some(f) = self.drag.take() {
                        let _ = ReleaseCapture();
                        if let Some((id, d)) = self.seek.take() {
                            self.send(media::Cmd::Seek(id, f * d));
                        }
                        self.touch();
                    }
                    if let Some((_, _, i)) = self.press.take() {
                        let _ = ReleaseCapture();
                        if let Some(e) = self.store.items.get(i).cloned() {
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
                    if self.drag.take().is_some() || self.press.take().is_some() {
                        self.touch();
                    }
                }
                WM_RBUTTONUP => {
                    if self.mode == Mode::Open {
                        let (x, y) = self.panel_pt(lp);
                        if let Hover::Row(i) = self.hit(x, y) {
                            self.store.remove(i);
                            self.flash = None;
                            self.store.save();
                            self.scroll = self.scroll.min(self.store.items.len().saturating_sub(self.rows()));
                            self.hover = Hover::None;
                            self.touch();
                        }
                    }
                }
                WM_MOUSEWHEEL => {
                    let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
                    // Precision touchpads send deltas under 120; still move one step.
                    let notches = if delta.abs() >= 120 { delta / 120 } else { delta.signum() };
                    if self.tab != Tab::Clip {
                        return Some(LRESULT(0));
                    }
                    let max = self.store.items.len().saturating_sub(self.rows()) as i32;
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
    CreateWindowExW(
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
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_GETMINMAXINFO | WM_SIZE | WM_MOVE | WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED => return DefWindowProcW(hwnd, msg, wp, lp),
        _ => {}
    }
    if let Some(r) = crate::app::island_message(msg, wp, lp) {
        return r;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
