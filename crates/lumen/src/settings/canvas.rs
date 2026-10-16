//! Small drawing kit for the settings window: text, rounded rectangles, toggles, glyphs.
//! Coordinates are DIPs; the render target scales them to the monitor.

use std::collections::HashMap;

use windows::core::{w, Result, HSTRING};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

pub type Color = D2D1_COLOR_F;

pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    D2D1_COLOR_F { r, g, b, a }
}

pub const fn white(a: f32) -> Color {
    rgba(1.0, 1.0, 1.0, a)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    Text,
    Display,
    Icons,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub struct Canvas {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: Option<ID2D1RenderTarget>,
    hwnd_target: Option<ID2D1HwndRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    formats: HashMap<(Font, u32, u32), IDWriteTextFormat>,
    families: HashMap<Font, HSTRING>,
    scale: f32,
}

impl Canvas {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut sys = None;
            dwrite.GetSystemFontCollection(&mut sys, false)?;
            let has = |name: &str| {
                let (mut i, mut e) = (0, BOOL(0));
                sys.as_ref().is_some_and(|c| c.FindFamilyName(&HSTRING::from(name), &mut i, &mut e).is_ok() && e.as_bool())
            };
            let pick = |a: &str, b: &str| HSTRING::from(if has(a) { a } else { b });
            let mut families = HashMap::new();
            families.insert(Font::Text, pick("Segoe UI Variable Text", "Segoe UI"));
            families.insert(Font::Display, pick("Segoe UI Variable Display", "Segoe UI"));
            families.insert(Font::Icons, pick("Segoe Fluent Icons", "Segoe MDL2 Assets"));
            Ok(Self { d2d, dwrite, target: None, hwnd_target: None, brush: None, formats: HashMap::new(), families, scale: 1.0 })
        }
    }

    /// Starts a frame on the window. Returns false if the target could not be made.
    pub fn begin(&mut self, hwnd: HWND, scale: f32, px: (u32, u32)) -> Result<()> {
        if self.scale != scale {
            self.hwnd_target = None;
            self.scale = scale;
        }
        if self.hwnd_target.is_none() {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
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
            self.hwnd_target = Some(unsafe { self.d2d.CreateHwndRenderTarget(&props, &hprops)? });
        }
        let t = self.hwnd_target.clone().unwrap();
        unsafe {
            let size = t.GetPixelSize();
            if size.width != px.0 || size.height != px.1 {
                t.Resize(&D2D_SIZE_U { width: px.0, height: px.1 })?;
            }
        }
        self.start(t.into())
    }

    /// Starts a frame on any render target (used for off-screen snapshots).
    pub fn start(&mut self, t: ID2D1RenderTarget) -> Result<()> {
        unsafe {
            t.BeginDraw();
            t.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            t.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
            self.brush = Some(t.CreateSolidColorBrush(&white(1.0), None)?);
        }
        self.target = Some(t);
        Ok(())
    }

    pub fn end(&mut self) -> Result<()> {
        self.brush = None;
        let Some(t) = self.target.take() else { return Ok(()) };
        let r = unsafe { t.EndDraw(None, None) };
        if r.as_ref().is_err_and(|e| e.code() == windows::Win32::Foundation::D2DERR_RECREATE_TARGET) {
            self.hwnd_target = None;
        }
        r
    }

    pub fn d2d(&self) -> &ID2D1Factory {
        &self.d2d
    }

    fn brush(&self, c: Color) -> Option<(&ID2D1RenderTarget, &ID2D1SolidColorBrush)> {
        let (t, b) = (self.target.as_ref()?, self.brush.as_ref()?);
        unsafe { b.SetColor(&c) };
        Some((t, b))
    }

    pub fn fill(&self, r: Rect, radius: f32, c: Color) {
        if let Some((t, b)) = self.brush(c) {
            unsafe { t.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }, b) };
        }
    }

    pub fn stroke(&self, r: Rect, radius: f32, width: f32, c: Color) {
        if let Some((t, b)) = self.brush(c) {
            let h = width / 2.0;
            let inner = Rect::new(r.x + h, r.y + h, r.w - width, r.h - width);
            unsafe { t.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: inner.d2d(), radiusX: radius, radiusY: radius }, b, width, None) };
        }
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, c: Color) {
        if let Some((t, b)) = self.brush(c) {
            let e = D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: r, radiusY: r };
            unsafe { t.FillEllipse(&e, b) };
        }
    }

    pub fn clip(&self, r: Rect) {
        if let Some(t) = &self.target {
            unsafe { t.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) };
        }
    }

    pub fn unclip(&self) {
        if let Some(t) = &self.target {
            unsafe { t.PopAxisAlignedClip() };
        }
    }

    fn format(&mut self, font: Font, size: f32, weight: u32) -> Option<IDWriteTextFormat> {
        let key = (font, size.to_bits(), weight);
        if let Some(f) = self.formats.get(&key) {
            return Some(f.clone());
        }
        unsafe {
            let f = self
                .dwrite
                .CreateTextFormat(&self.families[&font], None, DWRITE_FONT_WEIGHT(weight as i32), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))
                .ok()?;
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            if let Ok(sign) = self.dwrite.CreateEllipsisTrimmingSign(&f) {
                let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                let _ = f.SetTrimming(&trimming, &sign);
            }
            self.formats.insert(key, f.clone());
            Some(f)
        }
    }

    pub fn layout(&mut self, s: &str, font: Font, size: f32, weight: u32, w: f32, h: f32) -> Option<IDWriteTextLayout> {
        let f = self.format(font, size, weight)?;
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe { self.dwrite.CreateTextLayout(&wide, &f, w.max(1.0), h.max(1.0)).ok() }
    }

    /// Draws text vertically centred in `r`.
    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, s: &str, r: Rect, font: Font, size: f32, weight: u32, c: Color, align: Align) {
        let Some(l) = self.layout(s, font, size, weight, r.w, r.h) else { return };
        unsafe {
            let _ = l.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
        }
        if let Some((t, b)) = self.brush(c) {
            unsafe { t.DrawTextLayout(D2D_POINT_2F { x: r.x, y: r.y }, &l, b, D2D1_DRAW_TEXT_OPTIONS_NONE) };
        }
    }

    pub fn glyph(&mut self, g: char, r: Rect, size: f32, c: Color) {
        self.text(&g.to_string(), r, Font::Icons, size, 400, c, Align::Center);
    }

    pub fn measure(&mut self, s: &str, font: Font, size: f32, weight: u32) -> f32 {
        let Some(l) = self.layout(s, font, size, weight, 10_000.0, 100.0) else { return 0.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        m.widthIncludingTrailingWhitespace
    }

    /// x offset of character `utf16` within `s` (for the text caret).
    pub fn caret_x(&mut self, s: &str, utf16: u32, font: Font, size: f32) -> f32 {
        let Some(l) = self.layout(s, font, size, 400, 10_000.0, 100.0) else { return 0.0 };
        let (mut x, mut y) = (0f32, 0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = l.HitTestTextPosition(utf16, false, &mut x, &mut y, &mut m);
        }
        x
    }
}
