//! Start with Windows, via the per-user Run key (no admin rights, no scheduled task).

use windows::core::{w, HSTRING};
use windows::Win32::System::Registry::*;

const RUN: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

pub fn enabled() -> bool {
    unsafe {
        let mut size = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, RUN, w!("Lumen"), RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok()
    }
}

pub fn set(on: bool) -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN, 0, KEY_SET_VALUE, &mut key).is_err() {
            return false;
        }
        let ok = if on {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let value: Vec<u16> = format!("\"{exe}\"").encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            RegSetValueExW(key, w!("Lumen"), 0, REG_SZ, Some(bytes)).is_ok()
        } else {
            let r = RegDeleteValueW(key, &HSTRING::from("Lumen"));
            r.is_ok() || r.0 == 2 // already absent
        };
        let _ = RegCloseKey(key);
        crate::info!("start with Windows: {}", if on { "on" } else { "off" });
        ok
    }
}
