//! Shell icons loaded on a background thread.
//!
//! Asking Windows for a file's icon can take tens of milliseconds (shell extensions,
//! OneDrive, thumbnail handlers), so it must never happen on the thread that handles
//! typing. Rows show their type tile until the real icon arrives a moment later.
//! Ordinary files share one icon per extension, so most lookups are skipped entirely.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HPALETTE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub struct Pixels {
    pub w: u32,
    pub h: u32,
    /// Premultiplied BGRA rows.
    pub data: Vec<u8>,
}

pub struct IconLoader {
    tx: Sender<(String, String, i32)>,
    ready: Arc<Mutex<Vec<(String, Option<Pixels>)>>>,
    pending: HashSet<String>,
    /// Decoded icons survive the render target being released, so reopening is instant.
    cache: HashMap<String, Option<Arc<Pixels>>>,
}

/// Cache key: one per extension for plain files, per item for apps, programs and folders
/// with their own icons.
pub fn key_for(target: &str) -> String {
    if target.starts_with("shell:") {
        return target.to_string();
    }
    let p = Path::new(target);
    let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("exe" | "lnk" | "url" | "ico" | "appref-ms" | "msc" | "cpl" | "scr") => target.to_string(),
        Some(e) if !p.is_dir() => format!("ext:.{e}"),
        _ if target.len() <= 3 => target.to_string(), // drive roots have their own icons
        _ => "dir".to_string(),
    }
}

impl IconLoader {
    pub fn start(notify: HWND, msg: u32) -> Self {
        let (tx, rx) = channel::<(String, String, i32)>();
        let ready = Arc::new(Mutex::new(Vec::new()));
        let out = ready.clone();
        let notify = notify.0 as isize;
        let _ = std::thread::Builder::new().name("lumen-icons".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let wic: Option<IWICImagingFactory> = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
            while let Ok((key, target, px)) = rx.recv() {
                let pixels = wic.as_ref().and_then(|w| unsafe { load(w, &target, px) });
                if let Ok(mut r) = out.lock() {
                    r.push((key, pixels));
                }
                unsafe {
                    let _ = PostMessageW(HWND(notify as *mut _), msg, WPARAM(0), LPARAM(0));
                }
            }
        });
        Self { tx, ready, pending: HashSet::new(), cache: HashMap::new() }
    }

    /// The icon if it is loaded; otherwise queues it and returns None for now.
    pub fn get(&mut self, target: &str, px: i32) -> Option<Arc<Pixels>> {
        let key = key_for(target);
        if let Some(p) = self.cache.get(&key) {
            return p.clone();
        }
        if self.pending.insert(key.clone()) {
            let _ = self.tx.send((key, target.to_string(), px));
        }
        None
    }

    /// Moves finished loads into the cache. Returns true if anything arrived.
    pub fn collect(&mut self) -> bool {
        let done: Vec<_> = self.ready.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
        let any = !done.is_empty();
        for (key, pixels) in done {
            self.pending.remove(&key);
            if self.cache.len() > 300 {
                self.cache.clear();
            }
            self.cache.insert(key, pixels.map(Arc::new));
        }
        any
    }

    /// Icons were decoded for a different size (DPI change).
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

unsafe fn load(wic: &IWICImagingFactory, target: &str, px: i32) -> Option<Pixels> {
    let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(target), None).ok()?;
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let hbm = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
    let result = (|| -> windows::core::Result<Pixels> {
        let bmp = wic.CreateBitmapFromHBITMAP(hbm, HPALETTE::default(), WICBitmapUsePremultipliedAlpha)?;
        let scaled = wic.CreateBitmapScaler()?;
        scaled.Initialize(&bmp, px as u32, px as u32, WICBitmapInterpolationModeHighQualityCubic)?;
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(&scaled, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let (w, h) = (px as u32, px as u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut data)?;
        Ok(Pixels { w, h, data })
    })();
    let _ = DeleteObject(hbm);
    result.ok()
}
