//! Update check against GitHub releases. A background thread asks the API (via curl.exe,
//! which ships with Windows) for the latest release and posts `WM_UPDATE` to the app window.

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_UPDATE: u32 = WM_APP + 40;
/// Posted when the installer download finishes; wParam 1 = installer launched.
pub const WM_UPDATE_DONE: u32 = WM_APP + 41;
const API: &str = "https://api.github.com/repos/hasthij/Lumen/releases/latest";
const NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// Release notes contain `[important]`: nag with PLEASE UPDATE.
    pub important: bool,
}

/// The newest release found by the last check, if it is newer than this build.
pub static FOUND: Mutex<Option<Release>> = Mutex::new(None);

/// Check in the background; the app gets `WM_UPDATE` with wParam 1 when `manual` (so it can say "up to date").
pub fn check(hwnd: HWND, manual: bool) {
    let h = hwnd.0 as isize;
    std::thread::spawn(move || {
        *FOUND.lock().unwrap_or_else(|e| e.into_inner()) = latest().filter(|r| newer(&r.version, env!("CARGO_PKG_VERSION")));
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_UPDATE, WPARAM(manual as usize), LPARAM(0));
        }
    });
}

/// Download the installer to %TEMP% and return its path.
pub fn download(r: &Release) -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join("LumenSetup.exe");
    let ok = Command::new("curl.exe").args(["-L", "-f", "-s", "--connect-timeout", "15", "-m", "300", "-o"]).arg(&path).arg(&r.url).creation_flags(NO_WINDOW).status().ok()?.success();
    ok.then_some(path)
}

fn latest() -> Option<Release> {
    let out = Command::new("curl.exe").args(["-s", "-f", "-m", "15", "-H", "User-Agent: Lumen", "-H", "Accept: application/vnd.github+json", API]).creation_flags(NO_WINDOW).output().ok()?;
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// Pulls `tag_name` and the `LumenSetup.exe` download URL out of the release JSON without a JSON parser.
fn parse(json: &str) -> Option<Release> {
    let version = field(json, "tag_name")?.trim_start_matches('v').to_string();
    let url = json.split("\"browser_download_url\"").skip(1).filter_map(|s| s.split('"').nth(1)).find(|u| u.ends_with("LumenSetup.exe"))?.to_string();
    Some(Release { version, url, important: json.contains("[important]") })
}

fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let rest = json.split(&format!("\"{key}\"")).nth(1)?;
    rest.split('"').nth(1)
}

fn newer(a: &str, b: &str) -> bool {
    let n = |s: &str| s.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    n(a) > n(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.10", "0.2.2"));
        assert!(!newer("0.2.2", "0.2.2"));
        assert!(!newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn parses_release() {
        let j = r#"{"tag_name": "v0.3.0","assets":[{"browser_download_url": "https://x/a.zip"},{"browser_download_url": "https://x/LumenSetup.exe"}]}"#;
        let r = parse(j).unwrap();
        assert_eq!((r.version.as_str(), r.url.as_str(), r.important), ("0.3.0", "https://x/LumenSetup.exe", false));
        assert!(parse(&j.replace("\"assets\"", "\"body\": \"[important] fixes\", \"assets\"")).unwrap().important);
    }
}
