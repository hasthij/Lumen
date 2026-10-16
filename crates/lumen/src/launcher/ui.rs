//! Launcher drawing and layout. Coordinates are DIPs; Direct2D scales them to the monitor.

use std::collections::HashMap;

use windows::core::{w, Result, HSTRING};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use super::editor::Editor;
use super::item::{Action, Icon, Item};

pub const WIDTH: f32 = 680.0;
pub const SEARCH_H: f32 = 60.0;
pub const ROW_H: f32 = 52.0;
pub const LIST_PAD: f32 = 6.0;
pub const FOOTER_H: f32 = 38.0;
const ICON: f32 = 32.0;
const BUTTON: f32 = 28.0;

/// Window height for `rows` visible rows (0 = just the search box).
pub fn height(rows: usize) -> f32 {
    if rows == 0 {
        SEARCH_H
    } else {
        SEARCH_H + 1.0 + LIST_PAD * 2.0 + rows as f32 * ROW_H + 1.0 + FOOTER_H
    }
}

pub fn row_top(i: usize) -> f32 {
    SEARCH_H + 1.0 + LIST_PAD + i as f32 * ROW_H
}

/// Which visible row is at `y`, if any.
pub fn row_at(y: f32, rows: usize) -> Option<usize> {
    let top = row_top(0);
    if y < top || rows == 0 {
        return None;
    }
    let i = ((y - top) / ROW_H) as usize;
    (i < rows).then_some(i)
}

/// Action buttons on the selected row, right to left from the edge.
pub fn button_rect(row: usize, index: usize, count: usize) -> D2D_RECT_F {
    let top = row_top(row) + (ROW_H - BUTTON) / 2.0;
    let right = WIDTH - 20.0 - (count - 1 - index) as f32 * (BUTTON + 4.0);
    D2D_RECT_F { left: right - BUTTON, top, right, bottom: top + BUTTON }
}

pub fn button_at(x: f32, y: f32, row: usize, count: usize) -> Option<usize> {
    (0..count).find(|&i| {
        let r = button_rect(row, i, count);
        x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
    })
}

pub struct View<'a> {
    pub editor: &'a Editor,
    pub caret_on: bool,
    pub items: &'a [Item],
    pub first: usize,
    pub rows: usize,
    pub selected: usize,
    pub hover: Option<usize>,
    pub button_focus: Option<usize>,
    pub confirming: bool,
    pub chip: Option<&'a str>,
    pub message: Option<&'a str>,
    /// A Windows backdrop (Mica etc.) is behind the window.
    pub backdrop: bool,
    pub theme: crate::theme::Theme,
}

pub struct Renderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    loader: super::icons::IconLoader,
    target: Option<ID2D1HwndRenderTarget>,
    formats: HashMap<(u32, u32, bool), IDWriteTextFormat>,
    text_family: HSTRING,
    display_family: HSTRING,
    icon_family: HSTRING,
    /// D2D bitmaps for the current render target, made from the loader's pixels.
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    scale: f32,
    /// Horizontal scroll of the search text, so the caret stays visible.
    input_scroll: f32,
}

impl Renderer {
    pub fn new(hwnd: HWND, icon_msg: u32) -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut sys = None;
            dwrite.GetSystemFontCollection(&mut sys, false)?;
            let has = |name: &str| {
                let mut i = 0;
                let mut e = BOOL(0);
                sys.as_ref().is_some_and(|c| c.FindFamilyName(&HSTRING::from(name), &mut i, &mut e).is_ok() && e.as_bool())
            };
            let pick = |a: &str, b: &str| HSTRING::from(if has(a) { a } else { b });
            Ok(Self {
                text_family: pick("Segoe UI Variable Text", "Segoe UI"),
                display_family: pick("Segoe UI Variable Display", "Segoe UI"),
                icon_family: pick("Segoe Fluent Icons", "Segoe MDL2 Assets"),
                d2d,
                dwrite,
                loader: super::icons::IconLoader::start(hwnd, icon_msg),
                target: None,
                formats: HashMap::new(),
                bitmaps: HashMap::new(),
                scale: 1.0,
                input_scroll: 0.0,
            })
        }
    }

    /// Frees the render target and icons (the launcher is hidden; memory goes back to Windows).
    pub fn release(&mut self) {
        self.target = None;
        self.bitmaps.clear();
    }

    fn format(&mut self, size: f32, weight: u32, kind: u8) -> Result<IDWriteTextFormat> {
        let key = (size.to_bits(), weight, kind == 2);
        let key = (key.0, key.1 + kind as u32 * 10000, key.2);
        if let Some(f) = self.formats.get(&key) {
            return Ok(f.clone());
        }
        let family = match kind {
            0 => &self.text_family,
            1 => &self.display_family,
            _ => &self.icon_family,
        };
        unsafe {
            let f = self.dwrite.CreateTextFormat(
                family,
                None,
                DWRITE_FONT_WEIGHT(weight as i32),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )?;
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            // Long paths and titles end in "…" instead of overflowing.
            let sign = self.dwrite.CreateEllipsisTrimmingSign(&f)?;
            let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            f.SetTrimming(&trimming, &sign)?;
            self.formats.insert(key, f.clone());
            Ok(f)
        }
    }

    fn layout(&mut self, text: &str, size: f32, weight: u32, kind: u8, max_w: f32, h: f32) -> Result<IDWriteTextLayout> {
        let f = self.format(size, weight, kind)?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe { self.dwrite.CreateTextLayout(&wide, &f, max_w.max(1.0), h) }
    }

    fn target(&mut self, hwnd: HWND, scale: f32, px: (u32, u32)) -> Result<ID2D1HwndRenderTarget> {
        if self.scale != scale {
            self.target = None;
            self.bitmaps.clear();
            self.loader.clear();
            self.scale = scale;
        }
        if let Some(t) = &self.target {
            unsafe {
                let size = t.GetPixelSize();
                if size.width != px.0 || size.height != px.1 {
                    t.Resize(&D2D_SIZE_U { width: px.0, height: px.1 })?;
                }
            }
            return Ok(t.clone());
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            // Software: no GPU device or driver DLLs loaded for a small, rarely-drawn window.
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0 * scale,
            dpiY: 96.0 * scale,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width: px.0, height: px.1 },
            presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };
        let t = unsafe { self.d2d.CreateHwndRenderTarget(&props, &hprops)? };
        self.target = Some(t.clone());
        Ok(t)
    }

    /// Draws into an off-screen bitmap and returns premultiplied BGRA rows (for tests).
    pub fn render_offscreen(&mut self, scale: f32, px: (u32, u32), v: &View) -> Result<Vec<u8>> {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::*;
        self.release();
        self.scale = scale;
        // Snapshots wait for icons so the picture is complete.
        for _ in 0..2 {
            for it in v.items.iter().skip(v.first).take(v.rows) {
                if let Icon::Shell(target) = &it.icon {
                    let _ = self.loader.get(target, (ICON * scale).round() as i32);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
            self.loader.collect();
        }
        unsafe {
            let dc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: px.0 as i32,
                    biHeight: -(px.1 as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(dc, bmp);
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let t = self.d2d.CreateDCRenderTarget(&props)?;
            t.BindDC(dc, &RECT { left: 0, top: 0, right: px.0 as i32, bottom: px.1 as i32 })?;
            let r = self.draw_into(&t, v);
            let _ = GdiFlush();
            let out = std::slice::from_raw_parts(bits as *const u8, (px.0 * px.1 * 4) as usize).to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(dc);
            self.release();
            r.map(|_| out)
        }
    }

    pub fn draw(&mut self, hwnd: HWND, scale: f32, px: (u32, u32), v: &View) -> Result<()> {
        let t = self.target(hwnd, scale, px)?;
        let r = self.draw_into(&t, v);
        if let Err(e) = &r {
            if e.code() == windows::Win32::Foundation::D2DERR_RECREATE_TARGET {
                self.release();
            }
        }
        r
    }

    fn draw_into(&mut self, t: &ID2D1RenderTarget, v: &View) -> Result<()> {
        let h = height(if v.rows > 0 || v.message.is_some() { v.rows.max(1) } else { 0 });
        unsafe {
            t.BeginDraw();
            t.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            t.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
            let brush = t.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)?;
            let fill = |c: D2D1_COLOR_F, r: D2D_RECT_F, radius: f32| {
                brush.SetColor(&c);
                t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, &brush);
            };

            // Base: the theme tint over the backdrop, or solid when there is no backdrop.
            let [tr, tg, tb] = v.theme.tint;
            fill(rgba(tr, tg, tb, if v.backdrop { v.theme.strength } else { 0.98 }), rect(0.0, 0.0, WIDTH, h + 2.0), 0.0);

            self.draw_search(t, &brush, v)?;

            let show_list = v.rows > 0 || v.message.is_some();
            if show_list {
                fill(rgba(1.0, 1.0, 1.0, 0.07), rect(0.0, SEARCH_H, WIDTH, 1.0), 0.0);
            }
            if let Some(msg) = v.message.filter(|_| v.rows == 0) {
                brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.5));
                let l = self.layout(msg, 13.0, 400, 0, WIDTH - 40.0, ROW_H)?;
                t.DrawTextLayout(pt(20.0, row_top(0)), &l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }

            for row in 0..v.rows {
                let i = v.first + row;
                let Some(item) = v.items.get(i) else { break };
                self.draw_row(t, &brush, v, row, item, i == v.selected)?;
            }

            if v.rows > 0 {
                let ft = h - FOOTER_H;
                fill(rgba(1.0, 1.0, 1.0, 0.07), rect(0.0, ft - 1.0, WIDTH, 1.0), 0.0);
                if let Some(item) = v.items.get(v.selected) {
                    self.draw_footer(t, &brush, item, ft, v.confirming)?;
                }
            }
            t.EndDraw(None, None)
        }
    }

    unsafe fn draw_search(&mut self, t: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, v: &View) -> Result<()> {
        brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.55));
        let g = self.layout("\u{E721}", 18.0, 400, 2, 30.0, SEARCH_H)?;
        t.DrawTextLayout(pt(20.0, 0.0), &g, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);

        let left = 52.0;
        let mut right = WIDTH - 20.0;
        if let Some(chip) = v.chip {
            let l = self.layout(chip, 12.0, 500, 0, 300.0, 24.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            l.GetMetrics(&mut m)?;
            let w = m.width + 20.0;
            let r = rect(right - w, (SEARCH_H - 24.0) / 2.0, w, 24.0);
            let [ar, ag, ab] = v.theme.accent;
            brush.SetColor(&rgba(ar, ag, ab, 0.2));
            t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 12.0, radiusY: 12.0 }, brush);
            brush.SetColor(&rgba(ar, ag, ab, 1.0));
            t.DrawTextLayout(pt(r.left + 10.0, r.top), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            right -= w + 12.0;
        }
        let avail = right - left;

        let text = v.editor.text();
        t.PushAxisAlignedClip(&rect(left - 2.0, 0.0, avail + 4.0, SEARCH_H), D2D1_ANTIALIAS_MODE_ALIASED);
        if text.is_empty() {
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.38));
            let l = self.layout("Search apps, files, settings and more", 20.0, 400, 1, avail, SEARCH_H)?;
            t.DrawTextLayout(pt(left, 0.0), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            if v.caret_on {
                brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.9));
                t.FillRectangle(&rect(left, 18.0, 1.5, 24.0), brush);
            }
        } else {
            let l = self.layout(&text, 20.0, 400, 1, 10_000.0, SEARCH_H)?;
            let x_at = |i: usize| -> f32 {
                let (mut x, mut y) = (0f32, 0f32);
                let mut m = DWRITE_HIT_TEST_METRICS::default();
                let _ = l.HitTestTextPosition(v.editor.utf16_index(i), false, &mut x, &mut y, &mut m);
                x
            };
            let caret_x = x_at(v.editor.caret);
            if caret_x - self.input_scroll > avail - 4.0 {
                self.input_scroll = caret_x - avail + 4.0;
            } else if caret_x < self.input_scroll {
                self.input_scroll = caret_x;
            }
            let ox = left - self.input_scroll;
            if let Some((a, b)) = v.editor.selection() {
                let (xa, xb) = (x_at(a), x_at(b));
                let [ar, ag, ab] = v.theme.accent;
                brush.SetColor(&rgba(ar, ag, ab, 0.4));
                t.FillRectangle(&rect(ox + xa, 16.0, xb - xa, 28.0), brush);
            }
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.95));
            t.DrawTextLayout(pt(ox, 0.0), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            if v.caret_on {
                t.FillRectangle(&rect(ox + caret_x, 18.0, 1.5, 24.0), brush);
            }
        }
        t.PopAxisAlignedClip();
        Ok(())
    }

    unsafe fn draw_row(&mut self, t: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, v: &View, row: usize, item: &Item, selected: bool) -> Result<()> {
        let top = row_top(row);
        let hovered = v.hover == Some(row);
        if selected || hovered {
            brush.SetColor(&rgba(1.0, 1.0, 1.0, if selected { 0.08 } else { 0.04 }));
            let r = rect(8.0, top, WIDTH - 16.0, ROW_H);
            t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 8.0, radiusY: 8.0 }, brush);
        }
        if selected {
            // Accent pill on the left edge of the selected row.
            let [ar, ag, ab] = v.theme.accent;
            brush.SetColor(&rgba(ar, ag, ab, 1.0));
            let r = rect(8.0, top + 16.0, 3.0, ROW_H - 32.0);
            t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 1.5, radiusY: 1.5 }, brush);
        }

        let icon_rect = rect(20.0, top + (ROW_H - ICON) / 2.0, ICON, ICON);
        let bitmap = match &item.icon {
            Icon::Shell(target) => self.icon(t, target),
            _ => None,
        };
        match bitmap {
            Some(b) => t.DrawBitmap(&b, Some(&icon_rect), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
            None => {
                let [r, g, b] = item.kind.tint();
                brush.SetColor(&rgba(r, g, b, 0.24));
                t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: icon_rect, radiusX: 8.0, radiusY: 8.0 }, brush);
                let glyph = match item.icon {
                    Icon::Custom(c) => c,
                    _ => item.kind.glyph(),
                };
                let mix = |c: f32| c + (1.0 - c) * 0.45;
                brush.SetColor(&rgba(mix(r), mix(g), mix(b), 1.0));
                let l = self.layout(&glyph.to_string(), 16.0, 400, 2, ICON, ICON)?;
                l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                t.DrawTextLayout(pt(icon_rect.left, icon_rect.top), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }

        // Right side: action buttons on the selected row, the type tag elsewhere.
        let buttons = if selected { item.secondary.len().min(4) } else { 0 };
        let right_edge = if buttons > 0 {
            for (bi, action) in item.secondary.iter().take(4).enumerate() {
                let r = button_rect(row, bi, buttons);
                let focused = v.button_focus == Some(bi);
                brush.SetColor(&rgba(1.0, 1.0, 1.0, if focused { 0.16 } else { 0.06 }));
                t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 6.0, radiusY: 6.0 }, brush);
                if focused {
                    let [ar, ag, ab] = v.theme.accent;
                    brush.SetColor(&rgba(ar, ag, ab, 1.0));
                    t.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 6.0, radiusY: 6.0 }, brush, 1.0, None);
                }
                brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.85));
                let l = self.layout(&action.glyph().to_string(), 14.0, 400, 2, BUTTON, BUTTON)?;
                l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                t.DrawTextLayout(pt(r.left, r.top), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            button_rect(row, 0, buttons).left - 12.0
        } else {
            let l = self.layout(item.kind.tag(), 11.0, 400, 0, 200.0, 20.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            l.GetMetrics(&mut m)?;
            let w = m.width + 14.0;
            let r = rect(WIDTH - 20.0 - w, top + (ROW_H - 20.0) / 2.0, w, 20.0);
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.13));
            t.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 6.0, radiusY: 6.0 }, brush, 1.0, None);
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.55));
            t.DrawTextLayout(pt(r.left + 7.0, r.top), &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            r.left - 12.0
        };

        let text_left = 66.0;
        let max_w = right_edge - text_left;
        brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.95));
        let title = self.layout(&item.title, 14.5, 500, 0, max_w, 22.0)?;
        t.DrawTextLayout(pt(text_left, top + 6.0), &title, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        let (sub, color) = if selected && v.confirming {
            ("Press Enter again to confirm".to_string(), rgba(0.96, 0.58, 0.58, 1.0))
        } else {
            (item.subtitle.clone(), rgba(1.0, 1.0, 1.0, 0.55))
        };
        brush.SetColor(&color);
        let sub = self.layout(&sub, 12.0, 400, 0, max_w, 18.0)?;
        t.DrawTextLayout(pt(text_left, top + 27.0), &sub, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        Ok(())
    }

    unsafe fn draw_footer(&mut self, t: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, item: &Item, top: f32, confirming: bool) -> Result<()> {
        let mut hints: Vec<(&str, &str)> = vec![("Enter", if confirming { "Confirm" } else { item.primary.label() })];
        for a in item.secondary.iter().filter(|a| !matches!(a, Action::CloseWindow(_))).take(3) {
            hints.push((a.shortcut(), a.label()));
        }
        let mut x = WIDTH - 20.0;
        for (key, label) in hints.into_iter().rev() {
            let ll = self.layout(label, 11.5, 400, 0, 300.0, FOOTER_H)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            ll.GetMetrics(&mut m)?;
            x -= m.width;
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.55));
            t.DrawTextLayout(pt(x, top), &ll, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);

            let kl = self.layout(key, 10.5, 500, 0, 300.0, 18.0)?;
            kl.GetMetrics(&mut m)?;
            let kw = m.width + 10.0;
            x -= kw + 6.0;
            let r = rect(x, top + (FOOTER_H - 18.0) / 2.0, kw, 18.0);
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.09));
            t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 4.0, radiusY: 4.0 }, brush);
            brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.8));
            t.DrawTextLayout(pt(r.left + 5.0, r.top), &kl, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            x -= 16.0;
            if x < 140.0 {
                break;
            }
        }
        brush.SetColor(&rgba(1.0, 1.0, 1.0, 0.35));
        let brand = self.layout("Lumen", 11.5, 600, 0, 100.0, FOOTER_H)?;
        t.DrawTextLayout(pt(20.0, top), &brand, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        Ok(())
    }

    /// Shell icon as a bitmap if it has been loaded; otherwise asks the icon thread for it.
    unsafe fn icon(&mut self, t: &ID2D1RenderTarget, target: &str) -> Option<ID2D1Bitmap> {
        let key = super::icons::key_for(target);
        if let Some(b) = self.bitmaps.get(&key) {
            return b.clone();
        }
        let px = (ICON * self.scale).round() as i32;
        let pixels = self.loader.get(target, px)?;
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let size = D2D_SIZE_U { width: pixels.w, height: pixels.h };
        let bitmap = t.CreateBitmap(size, Some(pixels.data.as_ptr() as *const _), pixels.w * 4, &props).ok();
        self.bitmaps.insert(key, bitmap.clone());
        bitmap
    }

    /// Picks up icons the background thread finished. Returns true if a redraw is needed.
    pub fn icons_arrived(&mut self) -> bool {
        self.loader.collect()
    }
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}
