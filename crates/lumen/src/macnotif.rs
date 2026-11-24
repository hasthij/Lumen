//! MacNotif: Lumen's own notifications, shown in a screen corner and stacked like macOS.
//!
//! One layered, topmost, never-activating window covers the stack. Fully transparent pixels
//! let clicks through, so only the cards themselves take the mouse. Cards are drawn from
//! `Scene` items and animated by a timer that runs only while something is moving.
//! Clicking a card opens the app it came from (its window if it is running, else the app).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::core::{w, Result};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HALFTONE, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::MacNotifConfig;
use crate::gfx::{Backdrop, Fonts, LayeredSurface};
use crate::launcher::shell;
use crate::rainmeter::skin::{Item, Rgba, Scene, TextStyle};

/// Card height and layout constants, in 96-DPI pixels.
const CARD_H: f32 = 90.0;
/// Message lines a card can show; each one past the second makes the card taller.
const MAX_LINES: usize = 5;

fn card_h(lines: &[String]) -> f32 {
    CARD_H + lines.len().saturating_sub(2) as f32 * 16.0
}
const PAD: f32 = 10.0;
/// Room at the screen-edge end of the stack for the "Clear all" pill.
const TOP: f32 = 32.0;
const PEEK: f32 = 13.0;
const TIMER: usize = 1;
/// Posted once per display refresh by the pacer thread while cards are moving.
const WM_FRAME: u32 = WM_APP + 0x31;
static FAST: AtomicBool = AtomicBool::new(false);
static FRAME_PENDING: AtomicBool = AtomicBool::new(false);

/// What a program asks Lumen to show.
#[derive(Clone, Default)]
pub struct Notice {
    pub app: String,
    pub title: String,
    pub text: String,
    /// Program to open on click: a path, or a name of a running program (e.g. "brave").
    pub exe: String,
    /// Optional page to open in that program (goes to the browser's tab when it can).
    pub url: String,
    pub progress: Option<f32>,
    /// A picture to use as the icon instead of the app's (a chat avatar or group picture).
    pub image: String,
}

struct Card {
    id: u32,
    app: String,
    title: String,
    lines: Vec<String>,
    exe: String,
    url: String,
    icon: String,
    tile: Rgba,
    born: Instant,
    left_ms: f32,
    progress: Option<f32>,
    // Animated: distance from the screen-edge end, height, scale, opacity, slide-in.
    g: f32,
    h: f32,
    s: f32,
    a: f32,
    e: f32,
    leaving: bool,
    slab: bool,
    raw_title: String,
    raw_text: String,
}

#[derive(Clone, Copy)]
struct Target {
    /// Where the group's front card sits (g, h, s): cards that appear grow out from behind it.
    fr: (f32, f32, f32),
    g: f32,
    h: f32,
    s: f32,
    a: f32,
    slab: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    Clear,
    Close(usize),
    Card(usize),
}

#[derive(Clone, Copy)]
struct Geo {
    w: f32,
    right: bool,
    bottom: bool,
}

pub struct MacNotif {
    hwnd: HWND,
    fonts: Fonts,
    surface: LayeredSurface,
    cfg: MacNotifConfig,
    cards: Vec<Card>,
    next: u32,
    shown: bool,
    hovered: bool,
    hover_hit: Hit,
    suppressed: bool,
    fs_checked: Instant,
    last: Instant,
    timer_ms: u32,
    dirty: bool,
    scale: f32,
    scene_h: f32,
    scene_floor: f32,
    was_moving: bool,
    clear_rect: Option<(f32, f32, f32, f32)>,
    sample: usize,
    /// Screen rect and look of the blurred backdrop last captured; None when there is none.
    bd_key: Option<(i32, i32, u32, u32, u32, u32)>,
    /// A new capture is due (cards appeared again, or the look changed).
    bd_stale: bool,
    bd_at: Instant,
    /// Monitor the stack is on (HMONITOR); re-picked whenever the stack is empty.
    mon: isize,
}

impl MacNotif {
    pub fn create(cfg: &MacNotifConfig) -> Result<Self> {
        let hwnd = unsafe { create_window()? };
        if std::env::var_os("LUMEN_NOWIN").is_none() { crate::winnotif::start(hwnd, cfg.block_windows); }
        Ok(Self {
            hwnd,
            fonts: Fonts::new()?,
            surface: LayeredSurface::new()?,
            cfg: cfg.clone(),
            cards: Vec::new(),
            next: 1,
            shown: false,
            hovered: false,
            hover_hit: Hit::None,
            suppressed: false,
            fs_checked: Instant::now(),
            last: Instant::now(),
            timer_ms: 0,
            dirty: false,
            scale: dpi_scale(pick_monitor("primary")),
            scene_h: TOP + CARD_H,
            scene_floor: 0.0,
            was_moving: false,
            clear_rect: None,
            sample: 0,
            bd_key: None,
            bd_stale: true,
            bd_at: Instant::now(),
            mon: pick_monitor("primary").0 as isize,
        })
    }

    pub fn set_config(&mut self, cfg: &MacNotifConfig) {
        let rewrap = cfg.width != self.cfg.width || cfg.icon_size != self.cfg.icon_size;
        self.cfg = cfg.clone();
        if rewrap {
            let w = self.cfg.width as f32;
            for c in &mut self.cards {
                let (t, l) = wrap(&mut self.fonts, &c.raw_title, &c.raw_text, w, self.cfg.icon_size as f32);
                c.title = t;
                c.lines = l;
            }
        }
        self.fs_checked = Instant::now() - std::time::Duration::from_secs(1);
        self.dirty = true;
        self.bd_stale = true;
        self.set_timer(16);
        crate::winnotif::set_blocked(self.cfg.block_windows);
    }

    /// Adds a notification at the front of the stack.
    pub fn post(&mut self, n: Notice) {
        let exe = resolve_app(&n.exe);
        let icon = if !n.image.is_empty() {
            format!("icon:{}", n.image)
        } else if exe.is_empty() {
            String::new()
        } else {
            format!("icon:{exe}")
        };
        let app = if !n.app.is_empty() {
            n.app.chars().take(32).collect()
        } else if !exe.is_empty() {
            std::path::Path::new(&exe).file_stem().and_then(|s| s.to_str()).unwrap_or("Notification").to_string()
        } else {
            "Notification".to_string()
        };
        let (title, lines) = wrap(&mut self.fonts, &n.title, &n.text, self.cfg.width as f32, self.cfg.icon_size as f32);
        let h0 = card_h(&lines);
        if self.cards.len() >= 40 {
            self.cards.remove(0);
        }
        if self.cards.is_empty() {
            let m = pick_monitor(&self.cfg.monitor).0 as isize;
            if m != self.mon {
                self.mon = m;
                self.bd_stale = true;
            }
        }
        self.cards.push(Card {
            id: self.next,
            tile: tile_color(&app),
            app,
            title,
            lines,
            exe,
            url: n.url,
            icon,
            born: Instant::now(),
            left_ms: self.cfg.timeout_s as f32 * 1000.0,
            progress: n.progress,
            g: TOP,
            h: h0,
            s: 1.0,
            a: 1.0,
            e: 0.0,
            leaving: false,
            slab: false,
            raw_title: n.title,
            raw_text: n.text,
        });
        self.next += 1;
        // A new card starts where the layout wants it, then slides in.
        let targets = self.targets();
        if let Some(i) = self.cards.len().checked_sub(1) {
            if let Some((_, t)) = targets.iter().find(|(k, _)| *k == i) {
                let c = &mut self.cards[i];
                c.g = t.g;
                c.h = t.h;
                c.s = t.s;
                c.a = t.a;
            }
        }
        self.dirty = true;
        self.last = Instant::now();
        self.set_timer(16);
        if self.shown {
            unsafe {
                let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    /// The "Send test" notification: cycles through a few sample apps.
    pub fn post_sample(&mut self) {
        const SAMPLES: [(&str, &str, &str, [f32; 3]); 5] = [
            ("Messages", "Alex Rivera", "Are we still on for tonight? I booked a table for 8.", [0.20, 0.78, 0.35]),
            ("Mail", "Design review notes", "Priya shared 3 comments on the Q4 mockups.", [0.15, 0.50, 1.0]),
            ("Calendar", "Standup in 10 minutes", "Daily sync, Room 4. Join link attached.", [0.95, 0.27, 0.23]),
            ("Lumen", "Update ready", "Lumen is ready to install. Restart to finish.", [0.39, 0.40, 0.95]),
            ("Downloads", "Setup.exe", "Downloading, 64% of 212 MB", [1.0, 0.72, 0.20]),
        ];
        let (app, title, text, rgb) = SAMPLES[self.sample % SAMPLES.len()];
        let progress = (self.sample % SAMPLES.len() == 4).then_some(0.64);
        self.sample += 1;
        self.post(Notice { app: app.into(), title: title.into(), text: text.into(), progress, ..Default::default() });
        if let Some(c) = self.cards.last_mut() {
            c.tile = [rgb[0], rgb[1], rgb[2], 1.0];
            if app == "Lumen" {
                if let Ok(me) = std::env::current_exe() {
                    c.icon = format!("icon:{}", me.display());
                }
            }
        }
    }

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            crate::winnotif::WM_NOTE => {
                for n in crate::winnotif::drain() {
                    self.post(n);
                }
                Some(LRESULT(0))
            }
            WM_FRAME => {
                if FAST.load(Ordering::Relaxed) {
                    self.tick();
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER => {
                // While animating, the vsync pacer drives frames; the timer is only a fallback.
                if !FAST.load(Ordering::Relaxed) || self.last.elapsed().as_millis() > 40 {
                    self.tick();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !self.hovered {
                    self.hovered = true;
                    self.dirty = true;
                }
                self.hover_hit = self.hit_at(lp);
                self.set_timer(16);
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let hit = self.hit_at(lp);
                match hit {
                    Hit::Clear => self.cards.iter_mut().for_each(|c| c.leaving = true),
                    Hit::Close(i) => self.cards[i].leaving = true,
                    Hit::Card(i) => {
                        if msg == WM_LBUTTONDOWN {
                            activate(&self.cards[i].exe, &self.cards[i].url);
                        }
                        self.cards[i].leaving = true;
                    }
                    Hit::None => {}
                }
                self.dirty = true;
                self.set_timer(16);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn hit_at(&self, lp: LPARAM) -> Hit {
        let scale = self.scale.max(0.5);
        let x = (lp.0 & 0xffff) as i16 as f32 / scale;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32 / scale;
        self.hit(x, y)
    }

    fn geo(&self) -> Geo {
        Geo { w: self.cfg.width as f32, right: self.cfg.corner.ends_with('r'), bottom: self.cfg.corner.starts_with('b') }
    }

    /// Cards that are not on their way out, front of the stack first.
    fn order(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.cards.len()).filter(|&i| !self.cards[i].leaving).collect();
        if self.cfg.order != "old" {
            v.reverse();
        }
        v
    }

    fn fanned(&self) -> bool {
        self.cfg.mode == "tile" || self.hovered
    }

    fn targets(&self) -> Vec<(usize, Target)> {
        self.layout().0
    }

    /// One stack per app: each app's cards sit together (newest app first), stacks one below the
    /// other. Returns the card targets, how many cards are hidden, and where the last stack ends.
    fn layout(&self) -> (Vec<(usize, Target)>, usize, f32) {
        let fan = self.fanned();
        let max = self.cfg.max_shown.max(1) as usize;
        let gap = self.cfg.gap as f32;
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for idx in self.order() {
            let c = &self.cards[idx];
            let key = if c.exe.is_empty() { c.app.to_lowercase() } else { c.exe.to_lowercase() };
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(idx),
                None => groups.push((key, vec![idx])),
            }
        }
        let (mut out, mut hidden, mut y) = (Vec::new(), 0usize, TOP);
        for (gi, (_, members)) in groups.iter().enumerate() {
            let shown_group = gi < max;
            let n = members.len();
            let (mut off, mut fan_span) = (0.0f32, 0.0f32);
            let top_h = card_h(&self.cards[members[0]].lines);
            for (i, &idx) in members.iter().enumerate() {
                let ch = card_h(&self.cards[idx].lines);
                let t = if fan {
                    let vis = shown_group && i < max;
                    let g = y + off;
                    if vis {
                        off += ch + gap;
                        fan_span = off - gap;
                    }
                    Target { fr: (y, top_h, 1.0), g, h: ch, s: if vis { 1.0 } else { 0.9 }, a: if vis { 1.0 } else { 0.0 }, slab: false }
                } else {
                    let k = i.min(3) as f32;
                    let s = 1.0 - k * 0.06;
                    let h = ch * s;
                    Target { fr: (y, top_h, 1.0), g: y + top_h + k * PEEK - h, h, s, a: if i > 3 || !shown_group { 0.0 } else { 1.0 - k * 0.18 }, slab: i > 0 }
                };
                if t.a == 0.0 {
                    hidden += 1;
                }
                out.push((idx, t));
            }
            if shown_group {
                let span = if fan { fan_span } else { top_h + (n - 1).min(3) as f32 * PEEK };
                y += span + gap.max(10.0);
            }
        }
        (out, hidden, y - gap.max(10.0))
    }

    fn hit(&self, x: f32, y: f32) -> Hit {
        let inside = |r: (f32, f32, f32, f32)| x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3;
        if let Some(r) = self.clear_rect {
            if inside(r) {
                return Hit::Clear;
            }
        }
        let geo = self.geo();
        for i in self.order() {
            let c = &self.cards[i];
            if c.a < 0.5 || c.e < 0.4 {
                continue;
            }
            let r = card_rect(geo, c, self.scene_h);
            if inside(r) {
                return if self.hovered && x >= r.0 + r.2 - 34.0 && y <= r.1 + 34.0 { Hit::Close(i) } else { Hit::Card(i) };
            }
        }
        Hit::None
    }

    fn set_timer(&mut self, ms: u32) {
        FAST.store(ms == 16, Ordering::Relaxed);
        if ms == self.timer_ms {
            return;
        }
        unsafe {
            if ms == 0 {
                let _ = KillTimer(self.hwnd, TIMER);
            } else {
                SetTimer(self.hwnd, TIMER, ms, None);
            }
        }
        self.timer_ms = ms;
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.05);
        self.last = now;
        // The shell query can stall for tens of ms: never ask in the middle of an animation.
        if !self.was_moving && now.duration_since(self.fs_checked).as_millis() >= 500 {
            self.fs_checked = now;
            let hold = (!self.cfg.over_fullscreen && fullscreen_busy()) || (self.cfg.respect_focus && focus_on());
            if hold != self.suppressed {
                self.suppressed = hold;
                self.dirty = true;
            }
        }
        if self.cards.is_empty() {
            self.hovered = false;
            self.hide();
            self.set_timer(0);
            return;
        }
        if self.suppressed {
            self.hide();
            self.set_timer(500);
            return;
        }
        self.scale = dpi_scale(HMONITOR(self.mon as _));
        self.poll_hover();
        // The countdown waits while the mouse is over the stack.
        if !self.hovered && self.cfg.timeout_s > 0 {
            for c in self.cards.iter_mut().filter(|c| !c.leaving) {
                c.left_ms -= dt * 1000.0;
                if c.left_ms <= 0.0 {
                    c.leaving = true;
                }
            }
        }
        let moving = self.step(dt);
        self.cards.retain(|c| !(c.leaving && c.e < 0.03));
        if self.cards.is_empty() {
            self.hovered = false;
            self.hide();
            self.set_timer(0);
            return;
        }
        if moving || self.dirty {
            self.dirty = false;
            let t0 = Instant::now();
            self.render();
            if t0.elapsed().as_millis() > 8 {
                crate::warn!("macnotif frame took {} ms", t0.elapsed().as_millis());
            }
            // A slow first draw (loading an app icon) must not eat the slide-in.
            if t0.elapsed().as_millis() > 50 {
                self.last = Instant::now();
            }
        }
        // Live backdrop: refresh the blur a few times a second, only while the cards are at rest.
        if self.cfg.live_blur && self.cfg.blur > 0 && self.bd_at.elapsed().as_millis() >= if moving { 120 } else { 50 } {
            self.bd_stale = true;
            self.dirty = true;
        }
        self.was_moving = moving;
        let idle = if self.cfg.live_blur && self.cfg.blur > 0 { 30 } else { 100 };
        self.set_timer(if moving { 16 } else { idle });
    }

    /// Hover stays on while the mouse is anywhere over the fanned stack, gaps included.
    fn poll_hover(&mut self) {
        let mut p = POINT::default();
        let mut r = RECT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        let scale = self.scale.max(0.5);
        let (x, y) = ((p.x - r.left) as f32 / scale, (p.y - r.top) as f32 / scale);
        let (w, h) = ((r.right - r.left) as f32 / scale, (r.bottom - r.top) as f32 / scale);
        let on = if self.hovered { x >= 0.0 && x <= w && y >= 0.0 && y <= h } else { self.hit(x, y) != Hit::None };
        self.hover_hit = if on { self.hit(x, y) } else { Hit::None };
        if on != self.hovered {
            self.hovered = on;
            self.dirty = true;
        }
    }

    /// Moves every card toward its target. Returns true while anything is still moving.
    fn step(&mut self, dt: f32) -> bool {
        let k = 1.0 - (-dt / 0.045).exp();
        let ke = 1.0 - (-dt / 0.055).exp();
        let mut by_card: Vec<Option<Target>> = vec![None; self.cards.len()];
        for (i, t) in self.targets() {
            by_card[i] = Some(t);
        }
        let mut moving = false;
        for (i, c) in self.cards.iter_mut().enumerate() {
            let t = by_card[i];
            let (tg, th, ts, ta) = match t {
                Some(t) => {
                    c.slab = t.slab;
                    (t.g, t.h, t.s, t.a)
                }
                None => (c.g, c.h, c.s, 0.0),
            };
            let te = if c.leaving { 0.0 } else { 1.0 };
            // A card coming into view starts tucked behind its group's front card, not out of nowhere.
            if let Some(t) = t {
                if c.a < 0.02 && ta > 0.02 && !c.leaving && c.e > 0.9 {
                    c.g = t.fr.0;
                    c.h = t.fr.1;
                    c.s = t.fr.2 * 0.92;
                }
            }
            c.g += (tg - c.g) * k;
            c.h += (th - c.h) * k;
            c.s += (ts - c.s) * k;
            c.a += (ta - c.a) * k;
            c.e += (te - c.e) * ke;
            if (tg - c.g).abs() > 0.3 || (ts - c.s).abs() > 0.004 || (ta - c.a).abs() > 0.01 || (te - c.e).abs() > 0.01 {
                moving = true;
            } else {
                // Close enough: land exactly so the animation ends instead of crawling.
                (c.g, c.h, c.s, c.a, c.e) = (tg, th, ts, ta, te);
            }
        }
        moving
    }

    fn build(&mut self) -> Scene {
        let geo = self.geo();
        let live = self.order();
        // Rounded up in steps so the bitmap isn't reallocated every frame as the stack grows.
        // Sized for where the cards are going and never shrunk while visible, so the window (and its
        // blurred backdrop) isn't rebuilt mid-animation.
        let want_h = self.targets().iter().map(|(_, t)| t.g + t.h).chain(self.cards.iter().map(|c| c.g + c.h)).fold(TOP + CARD_H, f32::max);
        let scene_h = (((want_h + 40.0) / 128.0).ceil() * 128.0).max(self.scene_floor);
        self.scene_floor = scene_h;
        self.scene_h = scene_h;
        let scene_w = geo.w + PAD * 2.0;
        // A real blurred backdrop lets the tint go lighter; without one the card has to stay readable.
        let glass = self.cfg.blur > 0 && self.bd_key.is_some();
        let tint = (self.cfg.tint as f32 / 100.0).max(if glass { 0.35 } else { 0.72 });
        let radius = self.cfg.radius as f32;
        let mut items: Vec<Item> = Vec::new();

        // Back to front: cards on their way out, then the stack from the back forward.
        let mut draw_order: Vec<usize> = (0..self.cards.len()).filter(|&i| self.cards[i].leaving).collect();
        draw_order.extend(live.iter().rev().copied());
        let hover_idx = match self.hover_hit {
            Hit::Card(i) | Hit::Close(i) => Some(i),
            _ => None,
        };
        for &i in &draw_order {
            let c = &self.cards[i];
            let (x, y, w, h) = card_rect(geo, c, scene_h);
            let alpha = (c.a * (c.e * 1.6).min(1.0)).clamp(0.0, 1.0);
            if alpha < 0.01 {
                continue;
            }
            let fill = if c.slab { [52.0 / 255.0, 57.0 / 255.0, 70.0 / 255.0, 0.78 * alpha] } else { [22.0 / 255.0, 24.0 / 255.0, 30.0 / 255.0, tint * alpha] };
            // A soft drop shadow, faked with a larger, fainter rectangle under the card.
            items.push(Item::Rect { x: x + 4.0, y: y + 6.0, w: w - 8.0, h, radius: radius + 2.0, fill: [0.0, 0.0, 0.0, 0.22 * alpha], stroke: [0.0; 4], stroke_width: 0.0 });
            if glass {
                items.push(Item::Glass { x, y, w, h, radius, alpha });
            }
            items.push(Item::Rect { x, y, w, h, radius, fill, stroke: [1.0, 1.0, 1.0, 0.14 * alpha], stroke_width: 1.0 });
            if c.slab {
                continue;
            }
            let white = |a: f32| [1.0, 1.0, 1.0, a * alpha];
            // Icon tile: the app's own icon if there is one, else its initial on a colour.
            let (tx, ty) = (x + 14.0, y + 14.0);
            let isz = self.cfg.icon_size as f32;
            let bg = c.icon.is_empty() || self.cfg.icon_bg;
            if bg {
                items.push(Item::Rect { x: tx, y: ty, w: isz, h: isz, radius: isz * 0.25, fill: [c.tile[0], c.tile[1], c.tile[2], if c.icon.is_empty() { alpha } else { 0.18 * alpha }], stroke: [0.0; 4], stroke_width: 0.0 });
            }
            if c.icon.is_empty() {
                let st = style(isz * 0.47, 700);
                let initial: String = c.app.chars().next().unwrap_or('N').to_uppercase().collect();
                let (iw, ih) = self.fonts.measure(&initial, &st);
                items.push(Item::Text { text: initial, style: st, color: white(1.0), x: tx + (isz - iw) / 2.0, y: ty + (isz - ih) / 2.0, stretch: 1.0 });
            } else {
                let inset = if bg { isz / 12.0 } else { 0.0 };
                items.push(Item::Image { path: c.icon.clone(), x: tx + inset, y: ty + inset, w: isz - inset * 2.0, h: isz - inset * 2.0, alpha, tint: [1.0; 4] });
            }
            let tx = x + 24.0 + isz;
            items.push(Item::Text { text: c.app.to_uppercase(), style: style(10.5, 600), color: white(0.72), x: tx, y: y + 11.0, stretch: 1.0 });
            if hover_idx == Some(i) {
                items.push(Item::Rect { x: x + w - 28.0, y: y + 8.0, w: 18.0, h: 18.0, radius: 9.0, fill: white(0.16), stroke: [0.0; 4], stroke_width: 0.0 });
                let st = style(13.0, 600);
                let (cw, ch) = self.fonts.measure("\u{00d7}", &st);
                items.push(Item::Text { text: "\u{00d7}".into(), style: st, color: white(0.9), x: x + w - 19.0 - cw / 2.0, y: y + 17.0 - ch / 2.0, stretch: 1.0 });
            } else {
                let age = age_label(c.born);
                let st = style(10.5, 400);
                let (aw, _) = self.fonts.measure(&age, &st);
                items.push(Item::Text { text: age, style: st, color: white(0.62), x: x + w - 14.0 - aw, y: y + 11.0, stretch: 1.0 });
            }
            items.push(Item::Text { text: c.title.clone(), style: style(14.0, 700), color: white(0.96), x: tx, y: y + 27.0, stretch: 1.0 });
            for (n, line) in c.lines.iter().enumerate() {
                items.push(Item::Text { text: line.clone(), style: style(12.5, 400), color: white(0.86), x: tx, y: y + 47.0 + n as f32 * 16.0, stretch: 1.0 });
            }
            if let Some(p) = c.progress {
                let (bx, by, bw) = (x + 14.0, y + h - 11.0, w - 28.0);
                items.push(Item::Rect { x: bx, y: by, w: bw, h: 3.0, radius: 1.5, fill: white(0.14), stroke: [0.0; 4], stroke_width: 0.0 });
                items.push(Item::Rect { x: bx, y: by, w: (bw * p.clamp(0.0, 1.0)).max(3.0), h: 3.0, radius: 1.5, fill: white(0.85), stroke: [0.0; 4], stroke_width: 0.0 });
            }
        }

        // Pills: "Clear all" while hovering a stack, "+N more" when cards don't fit.
        self.clear_rect = None;
        let pill_x = |w: f32| if geo.right { PAD + geo.w - w } else { PAD };
        let pill_y = |g: f32, h: f32| if geo.bottom { scene_h - g - h } else { g };
        if self.hovered && live.len() > 1 {
            let (pw, ph) = (78.0, 24.0);
            let (px, py) = (pill_x(pw), pill_y(2.0, ph));
            self.clear_rect = Some((px, py, pw, ph));
            let st = style(11.5, 600);
            let (tw, th) = self.fonts.measure("Clear all", &st);
            items.push(Item::Rect { x: px, y: py, w: pw, h: ph, radius: 12.0, fill: [22.0 / 255.0, 24.0 / 255.0, 30.0 / 255.0, 0.85], stroke: [1.0, 1.0, 1.0, 0.16], stroke_width: 1.0 });
            items.push(Item::Text { text: "Clear all".into(), style: st, color: [1.0, 1.0, 1.0, 0.9], x: px + (pw - tw) / 2.0, y: py + (ph - th) / 2.0, stretch: 1.0 });
        }
        let (_, hidden, end_y) = self.layout();
        if hidden > 0 && self.fanned() {
            let label = format!("+{hidden} more");
            let st = style(11.5, 600);
            let (tw, th) = self.fonts.measure(&label, &st);
            let (pw, ph) = (tw + 24.0, 24.0);
            let (px, py) = (pill_x(pw), pill_y(end_y + 8.0, ph));
            items.push(Item::Rect { x: px, y: py, w: pw, h: ph, radius: 12.0, fill: [22.0 / 255.0, 24.0 / 255.0, 30.0 / 255.0, 0.85], stroke: [1.0, 1.0, 1.0, 0.16], stroke_width: 1.0 });
            items.push(Item::Text { text: label, style: st, color: [1.0, 1.0, 1.0, 0.8], x: px + 12.0, y: py + (ph - th) / 2.0, stretch: 1.0 });
        }
        Scene { items, width: scene_w.ceil() as u32, height: scene_h.ceil() as u32 }
    }

    fn render(&mut self) {
        let mut scene = self.build();
        let scale = self.scale;
        let wa = work_area(HMONITOR(self.mon as _));
        let (w, h) = crate::gfx::physical_size(&scene, scale);
        let m = (self.cfg.margin as f32 * scale).round() as i32;
        let pad = (PAD * scale).round() as i32;
        let geo = self.geo();
        let x = if geo.right { wa.right - m - w as i32 + pad } else { wa.left + m - pad };
        let y = if geo.bottom { wa.bottom - m - h as i32 } else { wa.top + m };
        // Blurred picture of the screen behind the stack, taken before the window is shown (and, because
        // layered windows are left out of a plain screen grab, never containing the cards themselves).
        let want = if self.cfg.blur > 0 { Some((x, y, w, h, self.cfg.blur, self.cfg.saturation)) } else { None };
        if want != self.bd_key || (want.is_some() && self.bd_stale) {
            let bd = want.and_then(|k| capture_backdrop(k, scale));
            self.bd_key = if bd.is_some() { want } else { None };
            self.surface.set_backdrop(bd);
            self.bd_stale = false;
            self.bd_at = Instant::now();
            if self.bd_key.is_some() != (self.cfg.blur > 0 && !scene.items.iter().all(|i| !matches!(i, Item::Glass { .. }))) {
                scene = self.build();
            }
        }
        match self.surface.present(self.hwnd, &mut self.fonts, &scene, scale, POINT { x, y }, 255) {
            Ok(()) => {
                if !self.shown {
                    unsafe {
                        let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                        let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                    self.shown = true;
                }
            }
            Err(e) => crate::error!("notification render failed: {e}"),
        }
    }

    fn hide(&mut self) {
        self.bd_stale = true;
        self.scene_floor = 0.0;
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
        self.dirty = true;
    }

    /// For `--macnotif-snapshot`: sample cards, settled, as premultiplied BGRA.
    pub fn snapshot(&mut self, count: usize, hover: bool) -> Option<(u32, u32, Vec<u8>)> {
        for _ in 0..count {
            self.post_sample();
        }
        self.hovered = hover;
        for _ in 0..200 {
            if !self.step(0.05) {
                break;
            }
        }
        for c in &mut self.cards {
            c.e = 1.0;
        }
        let scene = self.build();
        self.surface.render_pixels(&mut self.fonts, &scene, self.scale).ok()
    }
}

impl Drop for MacNotif {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER);
            let _ = DestroyWindow(self.hwnd);
        }
        crate::winnotif::set_blocked(false);
    }
}

fn card_rect(geo: Geo, c: &Card, scene_h: f32) -> (f32, f32, f32, f32) {
    let cw = geo.w * c.s;
    let slide = (1.0 - c.e) * (geo.w + PAD * 2.0);
    let x = PAD + (geo.w - cw) / 2.0 + if geo.right { slide } else { -slide };
    let y = if geo.bottom { scene_h - c.g - c.h } else { c.g };
    (x, y, cw, c.h)
}

fn style(size: f32, weight: u32) -> TextStyle {
    TextStyle { face: "Segoe UI".into(), size, weight, italic: false, spacing: [0.0; 3] }
}

/// Fits the title on one line and the text on up to two, with "…" where it runs out.
fn wrap(fonts: &mut Fonts, title: &str, text: &str, card_w: f32, icon: f32) -> (String, Vec<String>) {
    let avail = card_w - (icon + 38.0);
    let title = fit(fonts, title, &style(14.0, 700), avail);
    let st = style(12.5, 400);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if cur.is_empty() || fonts.measure(&cand, &st).0 <= avail {
            cur = cand;
        } else {
            lines.push(std::mem::replace(&mut cur, word.to_string()));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > MAX_LINES {
        let rest = lines[MAX_LINES - 1..].join(" ");
        lines.truncate(MAX_LINES - 1);
        lines.push(format!("{rest}\u{2026}"));
    }
    (title, lines.into_iter().map(|l| fit(fonts, &l, &st, avail)).collect())
}

fn fit(fonts: &mut Fonts, text: &str, st: &TextStyle, max_w: f32) -> String {
    if fonts.measure(text, st).0 <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().filter(|c| *c != '\u{2026}').collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "\u{2026}";
        if fonts.measure(&s, st).0 <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "\u{2026}"
}

fn age_label(born: Instant) -> String {
    let s = born.elapsed().as_secs();
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

/// A steady colour per app name, for apps with no icon.
fn tile_color(app: &str) -> Rgba {
    const PALETTE: [[f32; 3]; 6] = [[0.20, 0.78, 0.35], [0.15, 0.50, 1.0], [0.95, 0.27, 0.23], [0.39, 0.40, 0.95], [1.0, 0.72, 0.20], [0.70, 0.35, 0.90]];
    let h = app.bytes().fold(7usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize));
    let p = PALETTE[h % PALETTE.len()];
    [p[0], p[1], p[2], 1.0]
}

/// Grabs the screen rect (x, y, w, h in pixels), shrinks it by the blur radius, blurs and saturates it.
fn capture_backdrop(k: (i32, i32, u32, u32, u32, u32), scale: f32) -> Option<Backdrop> {
    let (x, y, w, h, blur, sat) = k;
    let d = (blur as f32 * scale / 2.45).max(2.0);
    let (sw, sh) = (((w as f32 / d) as i32).max(1), ((h as f32 / d) as i32).max(1));
    unsafe {
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
        let mut out = None;
        if let Ok(bmp) = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            let old = SelectObject(mem, bmp);
            SetStretchBltMode(mem, HALFTONE);
            if StretchBlt(mem, 0, 0, sw, sh, screen, x, y, w as i32, h as i32, SRCCOPY).as_bool() && !bits.is_null() {
                let _ = GdiFlush();
                let mut px = std::slice::from_raw_parts(bits as *const u8, (sw * sh * 4) as usize).to_vec();
                crate::island::blur_sat(&mut px, sw as usize, sh as usize, sat as f32 / 100.0);
                out = Some(Backdrop { w: sw as u32, h: sh as u32, px });
            }
            SelectObject(mem, old);
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        out
    }
}

fn pick_monitor(mode: &str) -> HMONITOR {
    unsafe {
        let origin = POINT { x: 0, y: 0 };
        match mode {
            "cursor" => {
                let mut p = origin;
                let _ = GetCursorPos(&mut p);
                MonitorFromPoint(p, MONITOR_DEFAULTTOPRIMARY)
            }
            "active" => {
                let fg = GetForegroundWindow();
                if fg.0.is_null() { MonitorFromPoint(origin, MONITOR_DEFAULTTOPRIMARY) } else { MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY) }
            }
            _ => MonitorFromPoint(origin, MONITOR_DEFAULTTOPRIMARY),
        }
    }
}

fn dpi_scale(mon: HMONITOR) -> f32 {
    unsafe {
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x >= 48 {
            x as f32 / 96.0
        } else {
            1.0
        }
    }
}

fn work_area(mon: HMONITOR) -> RECT {
    unsafe {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            info.rcWork
        } else {
            RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
        }
    }
}

/// A fullscreen app or presentation is in front (Windows' own "do not disturb" test).
/// Windows Focus (do not disturb) is on.
fn focus_on() -> bool {
    unsafe { matches!(SHQueryUserNotificationState(), Ok(s) if s == QUNS_QUIET_TIME) }
}

fn fullscreen_busy() -> bool {
    unsafe { matches!(SHQueryUserNotificationState(), Ok(s) if s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE) }
}

// ---- Opening the app a notification came from -------------------------------------------

/// Top-level windows a person would call "an app window", most recently used first.
fn app_windows() -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<HWND>);
        if IsWindowVisible(hwnd).as_bool() && GetWindow(hwnd, GW_OWNER).map(|o| o.is_invalid()).unwrap_or(true) && GetWindowTextLengthW(hwnd) > 0 {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 == 0 {
                out.push(hwnd);
            }
        }
        BOOL(1)
    }
    let mut out: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut Vec<HWND> as isize));
    }
    out
}

fn window_image(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    shell::process_image(pid)
}

/// "brave" or "C:\...\brave.exe" or "" -> the program's full path ("" if unknown).
fn resolve_app(app: &str) -> String {
    let app = app.trim();
    if app.is_empty() || app.starts_with("shell:") || std::path::Path::new(app).is_file() {
        return app.to_string();
    }
    let want = app.trim_end_matches(".exe").to_lowercase();
    for hwnd in app_windows() {
        if let Some(img) = window_image(hwnd) {
            let stem = std::path::Path::new(&img).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if stem == want {
                return img;
            }
        }
    }
    app.to_string()
}

/// Brings the program's window to the front, or starts it. With a link, the program opens
/// it (browsers reuse their window and show it in a tab).
fn activate(exe: &str, url: &str) {
    // Links come from other processes: only plain web links, with no quote to break the argument.
    let url = if (url.starts_with("http://") || url.starts_with("https://")) && !url.contains('"') { url } else { "" };
    if exe.is_empty() {
        if !url.is_empty() {
            shell::execute("open", url, "", None);
        }
        return;
    }
    if !url.is_empty() {
        shell::execute("open", exe, &format!("\"{url}\""), None);
        return;
    }
    if let Some(app_id) = exe.strip_prefix("shell:") {
        shell::execute("open", "explorer.exe", &format!("shell:{app_id}"), None);
        return;
    }
    let want = exe.to_lowercase();
    for hwnd in app_windows() {
        if window_image(hwnd).is_some_and(|img| img.to_lowercase() == want) {
            shell::switch_to(hwnd);
            return;
        }
    }
    shell::execute("open", exe, "", None);
}

// ---- Window ---------------------------------------------------------------------------

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("LumenMacNotif"),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    // Registering twice (module toggled off and on) fails harmlessly.
    RegisterClassExW(&class);
    let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
    let hwnd = CreateWindowExW(ex, w!("LumenMacNotif"), w!("Lumen Notifications"), WS_POPUP, 0, 0, 1, 1, None, None, instance, None)?;
    // Vsync pacer: SetTimer is coarse (15-31 ms), so wake the UI thread once per refresh while animating.
    let h = hwnd.0 as isize;
    std::thread::spawn(move || loop {
        let hw = HWND(h as *mut _);
        if !IsWindow(hw).as_bool() {
            break;
        }
        if FAST.load(Ordering::Relaxed) {
            let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            if !FRAME_PENDING.swap(true, Ordering::AcqRel) && PostMessageW(hw, WM_FRAME, WPARAM(0), LPARAM(0)).is_err() {
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
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_FRAME => {
            FRAME_PENDING.store(false, Ordering::Release);
            crate::app::macnotif_message(msg, wp, lp).unwrap_or(LRESULT(0))
        }
        crate::winnotif::WM_NOTE | WM_TIMER | WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_RBUTTONDOWN => crate::app::macnotif_message(msg, wp, lp).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp)),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
