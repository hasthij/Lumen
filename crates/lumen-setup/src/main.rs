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

use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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

const ID_GO: usize = 1;
const ID_AUTOSTART: usize = 2;
const ID_LAUNCH: usize = 3;
const ID_STATUS: usize = 4;

thread_local! {
    static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `/S`: install (or upgrade) with no window, then start Lumen. Otherwise show the setup window.
fn install(silent: bool) {
    if silent {
        if let Err(e) = do_install(false, false) {
            message(&format!("Couldn't install Lumen:

{e}"), MB_OK | MB_ICONERROR);
        }
        return;
    }
    unsafe { run_window() }
}

/// Copies the files and registers Lumen; the caller decides about autostart and launching.
fn do_install(autostart: bool, launch: bool) -> Result<(), String> {
    let dir = install_dir();
    stop_lumen();
    write_files(&dir)?;
    let _ = register(&dir);
    make_shortcut(&dir.join("lumen.exe"), &start_menu_link());
    if autostart {
        set_autostart(&dir.join("lumen.exe"));
    }
    if launch {
        let _ = Command::new(dir.join("lumen.exe")).arg("--settings").current_dir(&dir).spawn();
    } else {
        // A silent upgrade brings Lumen straight back, without opening settings.
        let _ = Command::new(dir.join("lumen.exe")).current_dir(&dir).spawn();
    }
    Ok(())
}

unsafe fn run_window() {
    let hi = GetModuleHandleW(None).unwrap_or_default();
    let class = w!("LumenSetupWindow");
    let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hi.into(), lpszClassName: class, hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _), ..Default::default() };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("Lumen Setup"), WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX, CW_USEDEFAULT, CW_USEDEFAULT, 480, 380, None, None, hi, None).unwrap_or_default();
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let upgrade = install_dir().join("lumen.exe").exists();
    let child = |class: PCWSTR, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: usize| {
        let c = CreateWindowExW(WINDOW_EX_STYLE(0), class, &HSTRING::from(text), WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style), x, y, w, h, hwnd, HMENU(id as *mut _), hi, None).unwrap_or_default();
        SendMessageW(c, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        c
    };
    let intro = if upgrade { "Lumen is already installed. Setup will update it and keep your settings." } else { "A desktop clock, launcher, clipboard and media island, and clear taskbar in one small app." };
    child(w!("STATIC"), intro, 0, 24, 112, 416, 36, 0);
    child(w!("STATIC"), &format!("Install to:  {}", install_dir().display()), 0, 24, 156, 416, 36, 0);
    let a = child(w!("BUTTON"), "Start Lumen when I sign in to Windows", BS_AUTOCHECKBOX as u32, 24, 204, 416, 24, ID_AUTOSTART);
    let l = child(w!("BUTTON"), "Open Lumen when setup finishes", BS_AUTOCHECKBOX as u32, 24, 232, 416, 24, ID_LAUNCH);
    SendMessageW(a, BM_SETCHECK, WPARAM(1), LPARAM(0));
    SendMessageW(l, BM_SETCHECK, WPARAM(1), LPARAM(0));
    child(w!("STATIC"), "", 0, 24, 272, 300, 24, ID_STATUS);
    child(w!("BUTTON"), if upgrade { "Update" } else { "Install" }, BS_DEFPUSHBUTTON as u32, 348, 292, 92, 30, ID_GO);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn checked(hwnd: HWND, id: usize) -> bool {
    SendMessageW(GetDlgItem(hwnd, id as i32).unwrap_or_default(), BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1
}

unsafe fn set_text(hwnd: HWND, id: usize, text: &str) {
    let _ = SetDlgItemTextW(hwnd, id as i32, &HSTRING::from(text));
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(hwnd, &mut ps);
    let _ = FillRect(dc, &RECT { left: 0, top: 0, right: 480, bottom: 96 }, GetSysColorBrush(COLOR_WINDOW));
    // Logo: black rounded tile with a centered orb.
    let tile = CreateSolidBrush(COLORREF(0x0010_1010));
    let orb = CreateSolidBrush(COLORREF(0x00F0_F0F0));
    let pen = GetStockObject(NULL_PEN);
    let (ob, op) = (SelectObject(dc, tile), SelectObject(dc, pen));
    let _ = RoundRect(dc, 24, 20, 80, 76, 16, 16);
    SelectObject(dc, orb);
    let _ = Ellipse(dc, 40, 36, 64, 60);
    SelectObject(dc, ob);
    SelectObject(dc, op);
    let _ = DeleteObject(tile);
    let _ = DeleteObject(orb);
    SetBkMode(dc, TRANSPARENT);
    let big = CreateFontW(-28, 0, 0, 0, 700, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, 0, w!("Segoe UI"));
    let small = CreateFontW(-14, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, 0, w!("Segoe UI"));
    let old = SelectObject(dc, big);
    let _ = TextOutW(dc, 96, 18, &"Lumen".encode_utf16().collect::<Vec<_>>());
    SelectObject(dc, small);
    SetTextColor(dc, COLORREF(0x0070_7070));
    let _ = TextOutW(dc, 98, 54, &format!("Version {VERSION}").encode_utf16().collect::<Vec<_>>());
    SelectObject(dc, old);
    let _ = DeleteObject(big);
    let _ = DeleteObject(small);
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_COMMAND if (wp.0 & 0xFFFF) == ID_GO => {
            if DONE.get() {
                if checked(hwnd, ID_LAUNCH) {
                    let _ = Command::new(install_dir().join("lumen.exe")).arg("--settings").current_dir(install_dir()).spawn();
                }
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            let (auto, launch) = (checked(hwnd, ID_AUTOSTART), checked(hwnd, ID_LAUNCH));
            let _ = EnableWindow(GetDlgItem(hwnd, ID_GO as i32).unwrap_or_default(), false);
            set_text(hwnd, ID_STATUS, "Installing...");
            // The launch checkbox is honored on Finish, so install without opening settings here.
            let _ = launch;
            match do_install(auto, false) {
                Ok(()) => {
                    DONE.set(true);
                    set_text(hwnd, ID_STATUS, "Lumen is installed and running.");
                    set_text(hwnd, ID_GO, "Finish");
                }
                Err(e) => set_text(hwnd, ID_STATUS, &format!("Failed: {e}")),
            }
            let _ = EnableWindow(GetDlgItem(hwnd, ID_GO as i32).unwrap_or_default(), true);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
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
        let _ = l.SetIconLocation(&HSTRING::from(target.as_os_str()), 0);
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
