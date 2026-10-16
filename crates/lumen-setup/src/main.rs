//! LumenSetup.exe: installs Lumen for the current user (no admin rights needed) and, when
//! started from the install folder as `uninstall.exe` (or with `--uninstall`), removes it.
//!
//! The program files are embedded in this exe, so it is the only file you need to share.
//! Build `lumen` and `lumen-tap` in release mode first (see scripts/package.ps1).

#![windows_subsystem = "windows"]

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::core::{w, Interface, HSTRING};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::*;

const LUMEN_EXE: &[u8] = include_bytes!("../../../target/release/lumen.exe");
const LUMEN_TAP: &[u8] = include_bytes!("../../../target/release/lumen_tap.dll");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Lumen";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let args: Vec<String> = std::env::args().collect();
    let exe_name = std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase())).unwrap_or_default();
    let silent = args.iter().any(|a| a == "/S" || a == "--silent");
    if exe_name == "uninstall" || args.iter().any(|a| a == "--uninstall") {
        uninstall(silent);
    } else {
        install(silent);
    }
}

fn message(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Lumen"), flags) }
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
    base.join("Programs").join("Lumen")
}

fn start_menu_link() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("Lumen.lnk")
}

/// Runs a command without flashing a console window.
fn quiet(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).creation_flags(CREATE_NO_WINDOW).status();
}

fn stop_lumen() {
    quiet("taskkill", &["/F", "/IM", "lumen.exe"]);
    std::thread::sleep(std::time::Duration::from_millis(600));
}

fn install(silent: bool) {
    if !silent {
        let ask = format!(
            "Install Lumen {VERSION}?\n\nLumen is a desktop clock, launcher and clear taskbar in one small app.\nIt installs for your account only and needs no administrator rights."
        );
        if message(&ask, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }
    let dir = install_dir();
    stop_lumen();
    if let Err(e) = write_files(&dir) {
        message(&format!("Couldn't install Lumen:\n\n{e}"), MB_OK | MB_ICONERROR);
        return;
    }
    if let Err(e) = register(&dir) {
        message(&format!("Lumen was copied, but couldn't be registered in Windows:\n\n{e}"), MB_OK | MB_ICONWARNING);
    }
    make_shortcut(&dir.join("lumen.exe"), &start_menu_link());

    let autostart = if silent { false } else { message("Start Lumen when you sign in to Windows?", MB_YESNO | MB_ICONQUESTION) == IDYES };
    if autostart {
        set_autostart(&dir.join("lumen.exe"));
    }
    // Opens the settings window on first launch so everything is easy to find.
    let _ = Command::new(dir.join("lumen.exe")).arg("--settings").current_dir(&dir).spawn();
    if !silent {
        message("Lumen is installed and running.\n\nLook for its icon in the tray. Click it for settings.", MB_OK | MB_ICONINFORMATION);
    }
}

fn write_files(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::write(dir.join("lumen.exe"), LUMEN_EXE).map_err(|e| format!("lumen.exe: {e}"))?;
    fs::write(dir.join("lumen_tap.dll"), LUMEN_TAP).map_err(|e| format!("lumen_tap.dll: {e}"))?;
    // The uninstaller is this same program under another name.
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = fs::copy(&me, dir.join("uninstall.exe"));
    Ok(())
}

fn set_reg(key: HKEY, name: &str, value: &str) {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe {
        let _ = RegSetValueExW(key, &HSTRING::from(name), 0, REG_SZ, Some(bytes));
    }
}

fn set_reg_dword(key: HKEY, name: &str, value: u32) {
    unsafe {
        let _ = RegSetValueExW(key, &HSTRING::from(name), 0, REG_DWORD, Some(&value.to_le_bytes()));
    }
}

/// The entry that shows up in Settings > Apps > Installed apps.
fn register(dir: &Path) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        let r = RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY), 0, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None);
        if r.is_err() {
            return Err(format!("registry: {r:?}"));
        }
        let uninstall = dir.join("uninstall.exe");
        set_reg(key, "DisplayName", "Lumen");
        set_reg(key, "DisplayVersion", VERSION);
        set_reg(key, "Publisher", "Lumen");
        set_reg(key, "InstallLocation", &dir.display().to_string());
        set_reg(key, "DisplayIcon", &dir.join("lumen.exe").display().to_string());
        set_reg(key, "UninstallString", &format!("\"{}\"", uninstall.display()));
        set_reg(key, "QuietUninstallString", &format!("\"{}\" /S", uninstall.display()));
        set_reg_dword(key, "NoModify", 1);
        set_reg_dword(key, "NoRepair", 1);
        let _ = RegCloseKey(key);
    }
    Ok(())
}

fn set_autostart(exe: &Path) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Run"), 0, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).is_ok() {
            set_reg(key, "Lumen", &format!("\"{}\"", exe.display()));
            let _ = RegCloseKey(key);
        }
    }
}

fn make_shortcut(target: &Path, link: &Path) {
    unsafe {
        let Ok(l): Result<IShellLinkW, _> = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return };
        let _ = l.SetPath(&HSTRING::from(target.as_os_str()));
        if let Some(dir) = target.parent() {
            let _ = l.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()));
        }
        let _ = l.SetArguments(w!("--settings"));
        let _ = l.SetDescription(w!("Lumen: desktop clock, launcher and clear taskbar"));
        if let Ok(file) = l.cast::<IPersistFile>() {
            if let Some(parent) = link.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = file.Save(&HSTRING::from(link.as_os_str()), true);
        }
    }
}

fn uninstall(silent: bool) {
    if !silent {
        let ask = "Remove Lumen from this PC?\n\nYour taskbar goes back to normal and Lumen stops starting with Windows.";
        if message(ask, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }
    let dir = install_dir();
    // Lumen puts the taskbar back, turns off autostart and unloads its helper from Explorer.
    let lumen = dir.join("lumen.exe");
    if lumen.exists() {
        let _ = Command::new(&lumen).arg("--uninstall").creation_flags(CREATE_NO_WINDOW).status();
    }
    stop_lumen();

    let _ = fs::remove_file(start_menu_link());
    unsafe {
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Run"), 0, KEY_SET_VALUE, &mut key).is_ok() {
            let _ = RegDeleteValueW(key, w!("Lumen"));
            let _ = RegCloseKey(key);
        }
    }

    let remove_settings = !silent && message("Also delete your Lumen settings and history?", MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION) == IDYES;
    if remove_settings {
        if let Some(a) = std::env::var_os("APPDATA") {
            let _ = fs::remove_dir_all(PathBuf::from(a).join("Lumen"));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            let _ = fs::remove_dir_all(PathBuf::from(l).join("Lumen"));
        }
    }

    // This program is running from the folder it has to delete, so a helper removes it a moment later.
    let target = dir.display().to_string();
    let _ = Command::new("cmd")
        .args(["/C", &format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{target}\"")])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if !silent {
        message("Lumen has been removed.", MB_OK | MB_ICONINFORMATION);
    }
}
