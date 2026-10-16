//! Launcher module: a PowerToys Run replacement.
//!
//! The window is created once and kept hidden, so opening is instant. While hidden it
//! holds no render target or icons (they are freed after 20 s), and its background
//! threads sleep on kernel events, so it costs nothing when idle.

pub mod apps;
pub mod editor;
pub mod fuzzy;
pub mod history;
pub mod icons;
pub mod item;
pub mod plugins;
pub mod shell;
pub mod ui;

use std::sync::Arc;
use std::time::Duration;

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Appearance, LauncherConfig};
use crate::theme::Theme;
use apps::Index;
use editor::Editor;
use history::History;
use item::{Action, Item, Kind};
use plugins::files::{Job, Worker};
use plugins::windows::OpenWindow;
use ui::{Renderer, View};

pub const WM_INDEX_CHANGED: u32 = WM_APP + 20;
pub const WM_SEARCH_RESULTS: u32 = WM_APP + 21;
pub const WM_ICONS_READY: u32 = WM_APP + 22;
const TIMER_CARET: usize = 1;
const TIMER_SEARCH: usize = 2;
const TIMER_RELEASE: usize = 3;

pub struct Launcher {
    hwnd: HWND,
    cfg: LauncherConfig,
    index: Arc<Index>,
    worker: Worker,
    history: History,
    renderer: Renderer,
    editor: Editor,
    items: Vec<Item>,
    sync_items: Vec<Item>,
    async_items: Vec<Item>,
    pending: Option<Vec<Job>>,
    searching: bool,
    gen: u64,
    selected: usize,
    first: usize,
    hover: Option<usize>,
    button_focus: Option<usize>,
    confirm_key: Option<String>,
    windows: Vec<OpenWindow>,
    visible: bool,
    caret_on: bool,
    /// Render scale: the monitor's DPI scaling times the user's size setting.
    scale: f32,
    /// The monitor's DPI scaling alone.
    dpi: f32,
    backdrop: bool,
    theme: Theme,
    high_surrogate: Option<u16>,
}

impl Launcher {
    pub fn create(cfg: &LauncherConfig, look: &Appearance) -> Result<Self> {
        let hwnd = unsafe { create_window()? };
        let backdrop = crate::theme::apply_backdrop(hwnd, look, true);
        Ok(Self {
            hwnd,
            cfg: cfg.clone(),
            index: Index::start(hwnd, WM_INDEX_CHANGED),
            worker: Worker::start(hwnd, WM_SEARCH_RESULTS),
            history: History::load(),
            renderer: Renderer::new(hwnd, WM_ICONS_READY)?,
            editor: Editor::default(),
            items: Vec::new(),
            sync_items: Vec::new(),
            async_items: Vec::new(),
            pending: None,
            searching: false,
            gen: 0,
            selected: 0,
            first: 0,
            hover: None,
            button_focus: None,
            confirm_key: None,
            windows: Vec::new(),
            visible: false,
            caret_on: true,
            scale: cfg.scale,
            dpi: 1.0,
            backdrop,
            theme: crate::theme::resolve(look),
            high_surrogate: None,
        })
    }

    pub fn set_config(&mut self, cfg: &LauncherConfig, look: &Appearance) {
        self.cfg = cfg.clone();
        self.scale = self.dpi * self.cfg.scale;
        self.resize();
        self.theme = crate::theme::resolve(look);
        self.backdrop = crate::theme::apply_backdrop(self.hwnd, look, true);
        self.redraw();
    }

    pub fn app_count(&self) -> usize {
        self.index.apps().len()
    }

    pub fn toggle(&mut self, from_hotkey: bool) {
        if self.visible {
            self.hide();
        } else {
            if from_hotkey && self.cfg.ignore_in_fullscreen && fullscreen_in_front() {
                crate::info!("launcher hotkey ignored: fullscreen app in front");
                return;
            }
            self.show();
        }
    }

    pub fn show(&mut self) {
        self.windows = plugins::windows::snapshot(self.hwnd);
        self.index.refresh_if_older(Duration::from_secs(30));
        self.editor.select_all();
        self.requery();
        unsafe {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let mon = MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut info);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            self.dpi = dx.max(48) as f32 / 96.0;
            self.scale = self.dpi * self.cfg.scale;
            let wa = info.rcWork;
            let (w, h) = self.pixel_size();
            let x = wa.left + ((wa.right - wa.left) - w as i32) / 2;
            let y = wa.top + ((wa.bottom - wa.top) as f32 * 0.2) as i32;
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w as i32, h as i32, SWP_NOACTIVATE);
            // A short fade in (120 ms, no extra frames from us; Windows does the blending).
            let _ = AnimateWindow(self.hwnd, 120, AW_BLEND);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
            let _ = KillTimer(self.hwnd, TIMER_RELEASE);
            SetTimer(self.hwnd, TIMER_CARET, GetCaretBlinkTime().clamp(300, 1200), None);
        }
        self.visible = true;
        self.caret_on = true;
        self.redraw();
    }

    pub fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.confirm_key = None;
        self.button_focus = None;
        self.hover = None;
        unsafe {
            let _ = AnimateWindow(self.hwnd, 90, AW_HIDE | AW_BLEND);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = KillTimer(self.hwnd, TIMER_CARET);
            SetTimer(self.hwnd, TIMER_RELEASE, 20_000, None);
        }
        self.windows.clear();
        self.history.save();
    }

    fn rows(&self) -> usize {
        self.items.len().min(self.cfg.max_results)
    }

    fn message(&self) -> Option<String> {
        let q = self.editor.text();
        (self.items.is_empty() && !q.trim().is_empty() && !self.searching).then(|| format!("No results for \u{201C}{}\u{201D}", q.trim()))
    }

    fn pixel_size(&self) -> (u32, u32) {
        let rows = if self.rows() > 0 || self.message().is_some() { self.rows().max(1) } else { 0 };
        ((ui::WIDTH * self.scale).round() as u32, (ui::height(rows) * self.scale).round() as u32)
    }

    fn resize(&self) {
        if !self.visible {
            return;
        }
        let (w, h) = self.pixel_size();
        unsafe {
            let mut r = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut r);
            if (r.bottom - r.top) as u32 != h || (r.right - r.left) as u32 != w {
                let _ = SetWindowPos(self.hwnd, None, 0, 0, w as i32, h as i32, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }

    fn redraw(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    /// Runs the fast plugins now and schedules the slow ones.
    fn requery(&mut self) {
        let started = std::time::Instant::now();
        self.requery_inner();
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        if ms > 16.0 {
            crate::warn!("launcher query took {ms:.1} ms for '{}'", self.editor.text());
        }
    }

    fn requery_inner(&mut self) {
        self.gen += 1;
        let text = self.editor.text();
        let apps = self.index.apps();
        let ctx = plugins::Context { cfg: &self.cfg, apps: &apps, windows: &self.windows };
        let (mut items, jobs) = plugins::query(&ctx, &text);
        for it in &mut items {
            it.score += self.history.boost(&it.key);
        }
        self.sync_items = items;
        self.async_items.clear();
        self.searching = !jobs.is_empty();
        self.worker.submit(self.gen, Vec::new());
        self.pending = (!jobs.is_empty()).then_some(jobs);
        if self.pending.is_some() {
            unsafe {
                SetTimer(self.hwnd, TIMER_SEARCH, 120, None);
            }
        }
        self.selected = 0;
        self.first = 0;
        self.button_focus = None;
        self.confirm_key = None;
        self.merge(None);
    }

    fn merge(&mut self, keep: Option<String>) {
        let mut all: Vec<Item> = self.sync_items.iter().chain(self.async_items.iter()).cloned().collect();
        all.sort_by(|a, b| b.score.cmp(&a.score));
        let mut seen = std::collections::HashSet::new();
        all.retain(|i| seen.insert(i.key.clone()));
        all.truncate(99);
        // "Search the web" sits at the bottom of what's visible, so it is always one glance
        // (or one Enter, when nothing else matched) away.
        if let Some(web) = self.web_fallback() {
            if !all.iter().any(|i| i.key == web.key) {
                let pos = if self.cfg.max_results <= 1 { all.len() } else { all.len().min(self.cfg.max_results - 1) };
                all.insert(pos, web);
            }
        }
        self.items = all;
        if let Some(k) = keep {
            if let Some(i) = self.items.iter().position(|it| it.key == k) {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.scroll_into_view();
        self.resize();
        self.redraw();
    }

    /// The web search row for what is typed, unless another plugin owns the query
    /// (a keyword such as "=" or ">") or a path is being browsed.
    fn web_fallback(&self) -> Option<Item> {
        let text = self.editor.text();
        let q = text.trim();
        if !self.cfg.web_fallback || q.is_empty() || plugins::files::is_path(q) {
            return None;
        }
        // With a keyword typed (including "??" for web) the plugin already answered.
        match plugins::route(&self.cfg, &text) {
            Some(_) => None,
            None => Some(plugins::web_item(&self.cfg, q)),
        }
    }

    fn scroll_into_view(&mut self) {
        let rows = self.rows().max(1);
        if self.selected < self.first {
            self.first = self.selected;
        } else if self.selected >= self.first + rows {
            self.first = self.selected + 1 - rows;
        }
        self.first = self.first.min(self.items.len().saturating_sub(rows));
    }

    fn select(&mut self, i: usize) {
        if self.items.is_empty() {
            return;
        }
        self.selected = i.min(self.items.len() - 1);
        self.button_focus = None;
        self.confirm_key = None;
        self.scroll_into_view();
        self.redraw();
    }

    fn move_selection(&mut self, delta: i32) {
        let n = self.items.len() as i32;
        if n == 0 {
            return;
        }
        self.select((self.selected as i32 + delta).rem_euclid(n) as usize);
    }

    fn run(&mut self, action: Action) {
        let Some(item) = self.items.get(self.selected).cloned() else { return };
        if item.confirm && action == item.primary && self.confirm_key.as_deref() != Some(item.key.as_str()) {
            self.confirm_key = Some(item.key.clone());
            self.redraw();
            return;
        }
        if let Action::Copy(text) = &action {
            if text.is_empty() {
                return;
            }
        }
        if item.kind != Kind::Web {
            self.history.record(&item.key);
        }
        // Hide first so focus goes back to where it was before the launched app takes it.
        self.hide();
        if !shell::run(&action, self.hwnd) {
            crate::warn!("action failed: {action:?}");
        }
    }

    fn secondary(&self, pred: impl Fn(&Action) -> bool) -> Option<Action> {
        self.items.get(self.selected).and_then(|i| i.find(pred)).cloned()
    }

    fn edited(&mut self, before: &str) {
        if self.editor.text() != before {
            self.requery();
        } else {
            self.redraw();
        }
        self.caret_on = true;
    }

    fn key_down(&mut self, vk: u16) -> bool {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
        let before = self.editor.text();
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                if self.confirm_key.take().is_some() || self.button_focus.take().is_some() {
                    self.redraw();
                } else {
                    self.hide();
                }
            }
            VK_RETURN => {
                let Some(item) = self.items.get(self.selected) else { return true };
                let action = match self.button_focus {
                    Some(b) => item.secondary.get(b).cloned(),
                    None if ctrl && shift => item.find(|a| matches!(a, Action::RunAsAdmin { .. })).cloned(),
                    None => None,
                }
                .unwrap_or_else(|| item.primary.clone());
                self.run(action);
            }
            VK_UP => self.move_selection(-1),
            VK_DOWN => self.move_selection(1),
            VK_PRIOR => self.move_selection(-(self.cfg.max_results as i32)),
            VK_NEXT => self.move_selection(self.cfg.max_results as i32),
            VK_TAB => {
                let n = self.items.get(self.selected).map_or(0, |i| i.secondary.len().min(4));
                if n > 0 {
                    self.button_focus = match (self.button_focus, shift) {
                        (None, false) => Some(0),
                        (None, true) => Some(n - 1),
                        (Some(b), false) => (b + 1 < n).then_some(b + 1),
                        (Some(b), true) => b.checked_sub(1),
                    };
                    self.redraw();
                }
            }
            VK_BACK => {
                self.editor.backspace(ctrl);
                self.edited(&before);
            }
            VK_DELETE => {
                self.editor.delete(ctrl);
                self.edited(&before);
            }
            VK_LEFT => {
                self.editor.left(ctrl, shift);
                self.edited(&before);
            }
            VK_RIGHT => {
                self.editor.right(ctrl, shift);
                self.edited(&before);
            }
            VK_HOME => {
                self.editor.move_to(0, shift);
                self.edited(&before);
            }
            VK_END => {
                let n = self.editor.len();
                self.editor.move_to(n, shift);
                self.edited(&before);
            }
            VK_E if ctrl && shift => {
                if let Some(a) = self.secondary(|a| matches!(a, Action::ShowInFolder(_))) {
                    self.run(a);
                }
            }
            VK_C if ctrl && shift => {
                if let Some(a) = self.secondary(|a| matches!(a, Action::Terminal(_))) {
                    self.run(a);
                }
            }
            VK_C if ctrl => match self.editor.selected_text() {
                Some(t) if self.editor.selection() != Some((0, self.editor.len())) || self.items.is_empty() => {
                    shell::copy(self.hwnd, &t);
                }
                _ => {
                    let copy = self.secondary(|a| matches!(a, Action::Copy(_))).or_else(|| match self.items.get(self.selected) {
                        Some(Item { primary: a @ Action::Copy(_), .. }) => Some(a.clone()),
                        _ => None,
                    });
                    if let Some(a) = copy {
                        self.run(a);
                    }
                }
            },
            VK_W if ctrl => {
                if let Some(a) = self.secondary(|a| matches!(a, Action::CloseWindow(_))) {
                    shell::run(&a, self.hwnd);
                    self.windows.retain(|w| !matches!(a, Action::CloseWindow(h) if h == w.hwnd));
                    self.requery();
                }
            }
            VK_A if ctrl => {
                self.editor.select_all();
                self.redraw();
            }
            VK_X if ctrl => {
                if let Some(t) = self.editor.selected_text() {
                    shell::copy(self.hwnd, &t);
                    self.editor.insert("");
                    self.edited(&before);
                }
            }
            VK_V if ctrl => {
                if let Some(t) = clipboard_text(self.hwnd) {
                    self.editor.insert(&t);
                    self.edited(&before);
                }
            }
            _ => return false,
        }
        true
    }

    fn char_input(&mut self, unit: u16) {
        let s = match unit {
            0xD800..=0xDBFF => {
                self.high_surrogate = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => match self.high_surrogate.take() {
                Some(hi) => String::from_utf16_lossy(&[hi, unit]),
                None => return,
            },
            // Control characters (Ctrl+letter, Enter, Esc, Tab, Backspace) are handled as keys.
            u if u < 0x20 || u == 0x7F => return,
            u => String::from_utf16_lossy(&[u]),
        };
        let before = self.editor.text();
        self.editor.insert(&s);
        self.edited(&before);
    }

    fn paint(&mut self) {
        let mut ps = PAINTSTRUCT::default();
        unsafe {
            BeginPaint(self.hwnd, &mut ps);
        }
        let message = self.message();
        let chip = plugins::route(&self.cfg, &self.editor.text()).map(|(id, _)| plugin_label(id));
        let view = View {
            editor: &self.editor,
            caret_on: self.caret_on,
            items: &self.items,
            first: self.first,
            rows: self.rows(),
            selected: self.selected,
            hover: self.hover,
            button_focus: self.button_focus,
            confirming: self.confirm_key.is_some(),
            chip,
            message: message.as_deref(),
            backdrop: self.backdrop,
            theme: self.theme,
        };
        let size = self.client_size();
        let started = std::time::Instant::now();
        if let Err(e) = self.renderer.draw(self.hwnd, self.scale, size, &view) {
            crate::warn!("launcher draw failed: {e}");
        }
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        if ms > 16.0 {
            crate::warn!("launcher frame took {ms:.1} ms");
        }
        unsafe {
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    fn client_size(&self) -> (u32, u32) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
        }
        ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
    }

    fn mouse_pos(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as i16 as f32 / self.scale;
        let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / self.scale;
        (x, y)
    }

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => self.paint(),
            WM_KEYDOWN => {
                if !self.key_down(wp.0 as u16) {
                    return None;
                }
            }
            WM_CHAR => self.char_input(wp.0 as u16),
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE && self.visible {
                    self.hide();
                }
                return None;
            }
            WM_CLOSE => self.hide(),
            WM_MOUSEMOVE => {
                let (_, y) = self.mouse_pos(lp);
                let hover = ui::row_at(y, self.rows());
                if hover != self.hover {
                    self.hover = hover;
                    self.redraw();
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
            }
            WM_MOUSELEAVE => {
                self.hover = None;
                self.redraw();
            }
            WM_LBUTTONUP => {
                let (x, y) = self.mouse_pos(lp);
                if let Some(row) = ui::row_at(y, self.rows()) {
                    let i = self.first + row;
                    let was_selected = i == self.selected;
                    self.select(i);
                    let item = self.items[i].clone();
                    let n = item.secondary.len().min(4);
                    match ui::button_at(x, y, row, n).filter(|_| was_selected) {
                        Some(b) => self.run(item.secondary[b].clone()),
                        None => self.run(item.primary.clone()),
                    }
                }
            }
            WM_MOUSEWHEEL => {
                let delta = (wp.0 >> 16) as i16;
                self.move_selection(if delta > 0 { -1 } else { 1 });
            }
            WM_TIMER => match wp.0 {
                TIMER_CARET => {
                    self.caret_on = !self.caret_on;
                    self.redraw();
                }
                TIMER_SEARCH => unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_SEARCH);
                    if let Some(jobs) = self.pending.take() {
                        self.worker.submit(self.gen, jobs);
                    }
                },
                TIMER_RELEASE => unsafe {
                    let _ = KillTimer(self.hwnd, TIMER_RELEASE);
                    self.renderer.release();
                    let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(windows::Win32::System::Threading::GetCurrentProcess());
                },
                _ => {}
            },
            WM_INDEX_CHANGED => {
                // New or removed apps: refresh what's on screen, keeping the selection.
                if self.visible {
                    let keep = self.items.get(self.selected).map(|i| i.key.clone());
                    self.requery();
                    self.merge(keep);
                }
            }
            WM_SEARCH_RESULTS => {
                if wp.0 as u64 == self.gen {
                    let results = self.worker.results.lock().ok().filter(|r| r.0 == self.gen).map(|r| r.1.clone());
                    if let Some(mut items) = results {
                        for it in &mut items {
                            it.score += self.history.boost(&it.key);
                        }
                        let keep = (self.selected > 0).then(|| self.items.get(self.selected).map(|i| i.key.clone())).flatten();
                        self.async_items = items;
                        self.searching = false;
                        self.merge(keep);
                    }
                }
            }
            WM_ICONS_READY => {
                if self.renderer.icons_arrived() && self.visible {
                    self.redraw();
                }
            }
            WM_ERASEBKGND => return Some(LRESULT(1)),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    /// For `--launcher-snapshot`: show off-screen with `query`, capture, and return BGRA rows.
    /// Runs outside the app's message routing, so it polls results itself.
    pub fn snapshot(&mut self, query: &str) -> Option<(u32, u32, Vec<u8>)> {
        self.windows = plugins::windows::snapshot(self.hwnd);
        self.dpi = 1.25;
        self.scale = self.dpi * self.cfg.scale;
        for _ in 0..100 {
            if !self.index.apps().is_empty() {
                break;
            }
            pump(50);
        }
        self.editor.set(query);
        self.requery();
        if let Some(jobs) = self.pending.take() {
            self.worker.submit(self.gen, jobs);
            for _ in 0..80 {
                pump(50);
                let done = self.worker.results.lock().ok().filter(|r| r.0 == self.gen).map(|r| r.1.clone());
                if let Some(items) = done {
                    self.async_items = items;
                    break;
                }
            }
        }
        self.searching = false;
        self.visible = true;
        self.merge(None);
        let (w, h) = self.pixel_size();
        let message = self.message();
        let chip = plugins::route(&self.cfg, &self.editor.text()).map(|(id, _)| plugin_label(id));
        let view = View {
            editor: &self.editor,
            caret_on: true,
            items: &self.items,
            first: self.first,
            rows: self.rows(),
            selected: self.selected,
            hover: None,
            button_focus: None,
            confirming: false,
            chip,
            message: message.as_deref(),
            backdrop: false,
            theme: self.theme,
        };
        let px = self.renderer.render_offscreen(self.scale, (w, h), &view);
        self.visible = false;
        px.ok().map(|p| (w, h, p))
    }

    /// Plain-text results, for `--query` testing.
    pub fn results_text(&mut self, query: &str) -> String {
        let _ = self.snapshot(query);
        self.items.iter().take(15).map(|i| format!("[{}] {} | {} (score {})\n", i.kind.tag(), i.title, i.subtitle, i.score)).collect()
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.history.save();
        self.index.shutdown();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn plugin_label(id: &str) -> &'static str {
    match id {
        "apps" => "Apps",
        "folder" => "Folders",
        "files" => "Files",
        "calculator" => "Calculator",
        "units" => "Unit converter",
        "time" => "Time and date",
        "values" => "Value generator",
        "windows" => "Windows",
        "shell" => "Command Prompt",
        "system" => "System",
        "settings" => "Settings",
        "services" => "Services",
        "registry" => "Registry",
        "web" => "Web search",
        _ => "",
    }
}

fn pump(ms: u64) {
    let end = std::time::Instant::now() + Duration::from_millis(ms);
    let mut msg = MSG::default();
    while std::time::Instant::now() < end {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A game or presentation is in front, so the hotkey should do nothing.
pub fn fullscreen_in_front() -> bool {
    unsafe {
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                return true;
            }
        }
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == GetShellWindow() || fg == GetDesktopWindow() {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(fg, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return false;
        }
        // Borderless fullscreen games: no caption and the window covers the whole monitor.
        let style = GetWindowLongW(fg, GWL_STYLE) as u32;
        if style & WS_CAPTION.0 == WS_CAPTION.0 {
            return false;
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(fg, &mut r);
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        let m = info.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

fn clipboard_text(owner: HWND) -> Option<String> {
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
        OpenClipboard(owner).ok()?;
        let text = (|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let p = GlobalLock(windows::Win32::Foundation::HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0;
            while *p.add(len) != 0 && len < 1 << 20 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        text
    }
}

unsafe fn create_window() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: w!("LumenLauncher"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        w!("LumenLauncher"),
        w!("Lumen"),
        WS_POPUP | WS_THICKFRAME,
        0,
        0,
        680,
        60,
        None,
        None,
        instance,
        None,
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Handled without touching launcher state: these arrive re-entrantly during SetWindowPos.
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => return LRESULT(0),
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_GETMINMAXINFO | WM_SIZE | WM_MOVE | WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED => {
            return DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => {}
    }
    if let Some(r) = crate::app::launcher_message(msg, wp, lp) {
        return r;
    }
    if msg == WM_CLOSE {
        // Never destroyed by Alt+F4; the app owns its lifetime.
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
