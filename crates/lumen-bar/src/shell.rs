//! Shell helpers: icons, shortcuts, launching, and per-window process/AppUserModelID lookup.

use windows::core::{w, Interface, Result, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, SIZE};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap1, ID2D1DeviceContext};
use windows::Win32::Graphics::Gdi::{DeleteObject, HPALETTE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::PropertiesSystem::SHGetPropertyStoreForWindow;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const APPS_FOLDER: &str = r"shell:AppsFolder\";

pub fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = HSTRING::from(s);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { ExpandEnvironmentStringsW(&src, Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n - 1])
}

pub fn file_name_lower(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase()
}

/// Resolves a .lnk to its target path (empty for shortcuts to non-file items).
pub fn resolve_lnk(path: &str) -> Option<String> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(&HSTRING::from(path), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
    }
}

pub fn launch(target: &str, args: &str) {
    let target = HSTRING::from(target);
    let args = HSTRING::from(args);
    let args = if args.is_empty() { PCWSTR::null() } else { PCWSTR(args.as_ptr()) };
    unsafe {
        ShellExecuteW(None, w!("open"), &target, args, PCWSTR::null(), SW_SHOWNORMAL);
    }
}

pub fn open_in_explorer(path: &str) {
    let args = HSTRING::from(format!("/select,\"{path}\""));
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), &args, PCWSTR::null(), SW_SHOWNORMAL);
    }
}

pub fn process_image(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

pub fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore =
            SHGetPropertyStoreForWindow(hwnd).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let s = value.to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Loads a shell icon (file path or shell:AppsFolder item) as a premultiplied D2D bitmap.
pub fn load_icon(dc: &ID2D1DeviceContext, wic: &IWICImagingFactory, target: &str, px: i32) -> Option<ID2D1Bitmap1> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(target), None).ok()?;
        let factory: IShellItemImageFactory = item.cast().ok()?;
        let hbm = factory.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let result = (|| -> Result<ID2D1Bitmap1> {
            let bmp = wic.CreateBitmapFromHBITMAP(hbm, HPALETTE::default(), WICBitmapUsePremultipliedAlpha)?;
            let conv = wic.CreateFormatConverter()?;
            conv.Initialize(&bmp, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            dc.CreateBitmapFromWicBitmap(&conv, None)
        })();
        let _ = DeleteObject(hbm);
        result.ok()
    }
}
