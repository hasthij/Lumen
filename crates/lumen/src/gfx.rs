//! Software Direct2D rendering into a per-pixel-alpha layered window, plus DirectWrite
//! font handling for skin fonts.
//!
//! The clock redraws once a minute, so a software (WARP-free, CPU) DC render target is the
//! lightest option: no D3D device, no GPU driver DLLs, no swap chain held in memory.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{w, Interface, Result, HSTRING};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::rainmeter::skin::{Item, Rgba, Scene, TextStyle};

/// One font file loaded into its own collection, so we know exactly which file a face is.
struct FontFile {
    collection: IDWriteFontCollection1,
    family: String,
    weight: DWRITE_FONT_WEIGHT,
    style: DWRITE_FONT_STYLE,
    stretch: DWRITE_FONT_STRETCH,
    /// Lower-case names this font answers to: family, full, Win32 family, PostScript, file stem.
    names: Vec<String>,
}

pub struct Fonts {
    dwrite: IDWriteFactory5,
    system: IDWriteFontCollection,
    files: Vec<FontFile>,
    formats: HashMap<(String, u32, bool, u32), IDWriteTextFormat>,
}

impl Fonts {
    pub fn new() -> Result<Self> {
        unsafe {
            let dwrite: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut system = None;
            let base: IDWriteFactory = dwrite.cast()?;
            base.GetSystemFontCollection(&mut system, false)?;
            let system = system.ok_or_else(windows::core::Error::empty)?;
            Ok(Self { dwrite, system, files: Vec::new(), formats: HashMap::new() })
        }
    }

    /// Loads every .ttf/.otf in `dir` (Rainmeter loads @Resources\Fonts automatically).
    /// Fonts are copied to a local cache first so OneDrive offloading or sync locks
    /// can never take the clock's fonts away.
    pub fn load_dir(&mut self, dir: &Path, cache: &Path) {
        self.files.clear();
        self.formats.clear();
        let Ok(entries) = std::fs::read_dir(dir) else {
            crate::warn!("no fonts folder at {}", dir.display());
            return self.load_cached_only(cache);
        };
        let _ = std::fs::create_dir_all(cache);
        for entry in entries.flatten() {
            let src = entry.path();
            if !is_font(&src) {
                continue;
            }
            let dst = cache.join(entry.file_name());
            let fresh = match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
                (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() <= b.modified().ok(),
                _ => false,
            };
            if !fresh {
                if let Err(e) = std::fs::copy(&src, &dst) {
                    crate::warn!("could not cache font {}: {e}", src.display());
                }
            }
            let path = if dst.exists() { dst } else { src };
            self.add_file(&path);
        }
    }

    fn load_cached_only(&mut self, cache: &Path) {
        let Ok(entries) = std::fs::read_dir(cache) else { return };
        for e in entries.flatten() {
            if is_font(&e.path()) {
                self.add_file(&e.path());
            }
        }
    }

    fn add_file(&mut self, path: &Path) {
        match unsafe { self.load_file(path) } {
            Ok(f) => {
                crate::info!("font {} -> '{}' w{} ({})", path.display(), f.family, f.weight.0, f.names.join(" | "));
                self.files.push(f);
            }
            Err(e) => crate::warn!("could not load font {}: {e}", path.display()),
        }
    }

    unsafe fn load_file(&self, path: &Path) -> Result<FontFile> {
        let file = self.dwrite.CreateFontFileReference(&HSTRING::from(path.as_os_str()), None)?;
        let builder: IDWriteFontSetBuilder1 = self.dwrite.CreateFontSetBuilder()?.cast()?;
        builder.AddFontFile(&file)?;
        let set = builder.CreateFontSet()?;
        let collection = self.dwrite.CreateFontCollectionFromFontSet(&set)?;
        let family = collection.GetFontFamily(0)?;
        let font = family.GetFont(0)?;
        let family_name = first_string(&family.GetFamilyNames()?).unwrap_or_default();

        let mut names = all_strings(&family.GetFamilyNames()?);
        for id in [
            DWRITE_INFORMATIONAL_STRING_FULL_NAME,
            DWRITE_INFORMATIONAL_STRING_WIN32_FAMILY_NAMES,
            DWRITE_INFORMATIONAL_STRING_POSTSCRIPT_NAME,
            DWRITE_INFORMATIONAL_STRING_TYPOGRAPHIC_FAMILY_NAMES,
        ] {
            let mut strings = None;
            let mut exists = BOOL(0);
            if font.GetInformationalStrings(id, &mut strings, &mut exists).is_ok() && exists.as_bool() {
                if let Some(s) = strings {
                    names.extend(all_strings(&s));
                }
            }
        }
        if let Some(stem) = path.file_stem() {
            names.push(stem.to_string_lossy().into_owned());
        }
        let mut names: Vec<String> = names.into_iter().map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()).collect();
        names.dedup();

        Ok(FontFile {
            collection,
            family: family_name,
            weight: font.GetWeight(),
            style: font.GetStyle(),
            stretch: font.GetStretch(),
            names,
        })
    }

    /// A text format for a Rainmeter FontFace, matched the way Rainmeter users expect:
    /// skin font by family, then by any of its names or file name, then a system font,
    /// then Segoe UI.
    pub fn format(&mut self, style: &TextStyle) -> Result<IDWriteTextFormat> {
        let key = (style.face.to_lowercase(), style.weight, style.italic, style.size.to_bits());
        if let Some(f) = self.formats.get(&key) {
            return Ok(f.clone());
        }
        let face = key.0.as_str();
        let want_style = if style.italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
        let want_weight = DWRITE_FONT_WEIGHT(style.weight as i32);

        let skin_font = self
            .files
            .iter()
            .find(|f| f.family.to_lowercase() == face)
            .map(|f| (f, false))
            .or_else(|| self.files.iter().find(|f| f.names.iter().any(|n| n == face)).map(|f| (f, true)));

        let format = unsafe {
            match skin_font {
                Some((f, by_alias)) => {
                    // Matched by full name ("Hero Bold"): use that face's own weight/style
                    // unless the skin explicitly asks for something heavier or italic.
                    let weight = if by_alias && style.weight == 400 { f.weight } else { want_weight };
                    let fstyle = if by_alias && !style.italic { f.style } else { want_style };
                    self.dwrite.CreateTextFormat(
                        &HSTRING::from(&f.family),
                        &f.collection,
                        weight,
                        fstyle,
                        f.stretch,
                        style.size,
                        w!("en-us"),
                    )?
                }
                None => {
                    let mut index = 0;
                    let mut exists = BOOL(0);
                    let name = HSTRING::from(&style.face);
                    let _ = self.system.FindFamilyName(&name, &mut index, &mut exists);
                    let family = if exists.as_bool() {
                        name
                    } else {
                        crate::warn!("font '{}' not found, using Segoe UI", style.face);
                        HSTRING::from("Segoe UI")
                    };
                    self.dwrite.CreateTextFormat(
                        &family,
                        &self.system,
                        want_weight,
                        want_style,
                        DWRITE_FONT_STRETCH_NORMAL,
                        style.size,
                        w!("en-us"),
                    )?
                }
            }
        };
        unsafe { format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)? };
        self.formats.insert(key, format.clone());
        Ok(format)
    }

    pub fn layout(&mut self, text: &str, style: &TextStyle) -> Result<IDWriteTextLayout> {
        let format = self.format(style)?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&wide, &format, 16384.0, 16384.0)?;
            if style.spacing != [0.0; 3] {
                let l1: IDWriteTextLayout1 = layout.cast()?;
                let range = DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 };
                l1.SetCharacterSpacing(style.spacing[0], style.spacing[1], style.spacing[2], range)?;
            }
            Ok(layout)
        }
    }

    /// Pixel width and height of a string, as the String meter will draw it.
    pub fn measure(&mut self, text: &str, style: &TextStyle) -> (f32, f32) {
        let Ok(layout) = self.layout(text, style) else { return (0.0, 0.0) };
        let mut m = DWRITE_TEXT_METRICS::default();
        match unsafe { layout.GetMetrics(&mut m) } {
            Ok(()) => (m.widthIncludingTrailingWhitespace, m.height),
            Err(_) => (0.0, 0.0),
        }
    }
}

fn is_font(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("ttf" | "otf" | "ttc"))
}

unsafe fn all_strings(s: &IDWriteLocalizedStrings) -> Vec<String> {
    (0..s.GetCount()).filter_map(|i| string_at(s, i)).collect()
}

unsafe fn first_string(s: &IDWriteLocalizedStrings) -> Option<String> {
    // Prefer en-us, fall back to the first entry.
    let mut index = 0;
    let mut exists = BOOL(0);
    let _ = s.FindLocaleName(w!("en-us"), &mut index, &mut exists);
    string_at(s, if exists.as_bool() { index } else { 0 })
}

unsafe fn string_at(s: &IDWriteLocalizedStrings, i: u32) -> Option<String> {
    let len = s.GetStringLength(i).ok()? as usize;
    let mut buf = vec![0u16; len + 1];
    s.GetString(i, &mut buf).ok()?;
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// A 32-bit premultiplied DIB that Direct2D draws into and UpdateLayeredWindow presents.
pub struct LayeredSurface {
    d2d: ID2D1Factory,
    target: Option<ID2D1DCRenderTarget>,
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    old: HGDIOBJ,
    width: u32,
    height: u32,
}

impl LayeredSurface {
    pub fn new() -> Result<Self> {
        let d2d: ID2D1Factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(windows::core::Error::from_win32());
        }
        Ok(Self { d2d, target: None, dc, bitmap: HBITMAP::default(), bits: std::ptr::null_mut(), old: HGDIOBJ::default(), width: 0, height: 0 })
    }

    fn ensure_size(&mut self, width: u32, height: u32) -> Result<()> {
        if width == self.width && height == self.height && !self.bitmap.is_invalid() {
            return Ok(());
        }
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let previous = SelectObject(self.dc, bitmap);
            if self.bitmap.is_invalid() {
                self.old = previous;
            } else {
                let _ = DeleteObject(self.bitmap);
            }
            self.bitmap = bitmap;
            self.bits = bits as *mut u8;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn target(&mut self) -> Result<ID2D1DCRenderTarget> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let t = unsafe { self.d2d.CreateDCRenderTarget(&props)? };
        self.target = Some(t.clone());
        Ok(t)
    }

    /// Draws `scene` (in skin pixels) at `scale` and shows it at `pos` (screen pixels)
    /// with the given window opacity.
    pub fn present(&mut self, hwnd: HWND, fonts: &mut Fonts, scene: &Scene, scale: f32, pos: POINT, alpha: u8) -> Result<()> {
        let (w, h) = physical_size(scene, scale);
        self.ensure_size(w, h)?;
        if let Err(e) = self.draw(fonts, scene, scale) {
            // Device or target trouble: drop the target and try once more from scratch.
            crate::warn!("draw failed ({e}), recreating render target");
            self.target = None;
            self.draw(fonts, scene, scale)?;
        }
        let size = SIZE { cx: self.width as i32, cy: self.height as i32 };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            UpdateLayeredWindow(hwnd, None, Some(&pos), Some(&size), self.dc, Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA)
        }
    }

    /// Renders without presenting and returns premultiplied BGRA rows (for snapshots).
    pub fn render_pixels(&mut self, fonts: &mut Fonts, scene: &Scene, scale: f32) -> Result<(u32, u32, Vec<u8>)> {
        let (w, h) = physical_size(scene, scale);
        self.ensure_size(w, h)?;
        self.draw(fonts, scene, scale)?;
        let _ = unsafe { windows::Win32::Graphics::Gdi::GdiFlush() };
        let len = (self.width * self.height * 4) as usize;
        let pixels = unsafe { std::slice::from_raw_parts(self.bits, len) }.to_vec();
        Ok((self.width, self.height, pixels))
    }

    fn draw(&mut self, fonts: &mut Fonts, scene: &Scene, scale: f32) -> Result<()> {
        let target = self.target()?;
        let rect = RECT { left: 0, top: 0, right: self.width as i32, bottom: self.height as i32 };
        unsafe {
            // Skin coordinates are 96-DPI pixels; Direct2D scales them to the display.
            target.SetDpi(96.0 * scale, 96.0 * scale);
            target.BindDC(self.dc, &rect)?;
            target.BeginDraw();
            target.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let brush = target.CreateSolidColorBrush(&color(&[1.0; 4]), None)?;

            for item in &scene.items {
                match item {
                    Item::Text { text, style, color: c, x, y, .. } => {
                        if let Ok(layout) = fonts.layout(text, style) {
                            brush.SetColor(&color(c));
                            target.DrawTextLayout(D2D_POINT_2F { x: *x, y: *y }, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                        }
                    }
                    Item::Rect { x, y, w, h, radius, fill, stroke, stroke_width } => {
                        let r = D2D_RECT_F { left: *x, top: *y, right: x + w, bottom: y + h };
                        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: *radius, radiusY: *radius };
                        if fill[3] > 0.0 {
                            brush.SetColor(&color(fill));
                            target.FillRoundedRectangle(&rr, &brush);
                        }
                        if stroke[3] > 0.0 && *stroke_width > 0.0 {
                            brush.SetColor(&color(stroke));
                            target.DrawRoundedRectangle(&rr, &brush, *stroke_width, None);
                        }
                    }
                }
            }
            target.EndDraw(None, None)
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.bitmap.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.bitmap);
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

pub fn physical_size(scene: &Scene, scale: f32) -> (u32, u32) {
    ((scene.width as f32 * scale).ceil().max(1.0) as u32, (scene.height as f32 * scale).ceil().max(1.0) as u32)
}

fn color(c: &Rgba) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c[0], g: c[1], b: c[2], a: c[3] }
}

/// Cache folder for a skin's fonts.
pub fn font_cache_dir(skin_root_name: &str) -> PathBuf {
    crate::log::dir().join("fonts").join(skin_root_name)
}

