//! Lumen: an all-in-one Windows customization app.
//!
//! Modules: desktop clock (native Rainmeter skin renderer) and launcher (PowerToys Run
//! replacement). Clear taskbar comes next.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod clock;
mod config;
mod gfx;
mod icon;
mod island;
mod launcher;
mod macnotif;
#[macro_use]
mod log;
mod rainmeter;
mod settings;
mod taskbar;
mod theme;
mod update;
mod winnotif;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW};

fn main() {
    log::install_panic_hook();
    let args: Vec<String> = std::env::args().collect();

    unsafe {
        // The shell APIs used for icons, launching and Explorer need an STA thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // Test helpers: render to a file or print results, then exit.
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "clock.bmp".into());
        std::process::exit(snapshot(&out));
    }
    if let Some(i) = args.iter().position(|a| a == "--launcher-snapshot") {
        let q = args.get(i + 1).cloned().unwrap_or_default();
        let out = args.get(i + 2).cloned().unwrap_or_else(|| "launcher.bmp".into());
        std::process::exit(launcher_snapshot(&q, &out));
    }
    if let Some(i) = args.iter().position(|a| a == "--island-snapshot") {
        let tab = args.get(i + 1).cloned().unwrap_or_default();
        let out = args.get(i + 2).cloned().unwrap_or_else(|| "island.bmp".into());
        let cfg = config::Config::load();
        let Ok(mut isl) = island::Island::create(&cfg.island) else { std::process::exit(1) };
        let code = match isl.snapshot(&tab) {
            Some((w, h, px)) => write_bmp(&out, w, h, &px),
            None => 2,
        };
        std::process::exit(code);
    }
    // `--macnotif-snapshot <count> <hover 0|1> <out.bmp>`: sample notifications, settled.
    if let Some(i) = args.iter().position(|a| a == "--macnotif-snapshot") {
        let count = args.get(i + 1).and_then(|a| a.parse().ok()).unwrap_or(3);
        let hover = args.get(i + 2).is_some_and(|a| a == "1");
        let out = args.get(i + 3).cloned().unwrap_or_else(|| "macnotif.bmp".into());
        let cfg = config::Config::load();
        let Ok(mut m) = macnotif::MacNotif::create(&cfg.macnotif) else { std::process::exit(1) };
        let code = match m.snapshot(count, hover) {
            Some((w, h, px)) => write_bmp(&out, w, h, &px),
            None => 2,
        };
        std::process::exit(code);
    }
    // `lumen --notify "title" "text" [--app <exe|name>] [--url <link>]`: show a notification in the running Lumen.
    if let Some(i) = args.iter().position(|a| a == "--notify") {
        let opt = |name: &str| args.iter().position(|a| a == name).and_then(|j| args.get(j + 1)).cloned().unwrap_or_default();
        let title = args.get(i + 1).filter(|a| !a.starts_with("--")).cloned().unwrap_or_default();
        let text = args.get(i + 2).filter(|a| !a.starts_with("--")).cloned().unwrap_or_default();
        std::process::exit(send_notice(&title, &text, &opt("--app"), &opt("--url")));
    }
    if let Some(i) = args.iter().position(|a| a == "--query") {
        let q = args.get(i + 1).cloned().unwrap_or_default();
        let cfg = config::Config::load();
        match launcher::Launcher::create(&cfg.launcher, &cfg.appearance) {
            Ok(mut l) => print!("{}", l.results_text(&q)),
            Err(e) => eprintln!("{e}"),
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--settings-snapshot") {
        let page = match args.get(i + 1).map(String::as_str) {
            Some("clock") | Some("skins") => settings::Page::Skins,
            Some("appearance") => settings::Page::Appearance,
            Some("island") => settings::Page::Island,
            Some("macnotif") | Some("notifications") => settings::Page::MacNotif,
            Some("taskbar") => settings::Page::Taskbar,
            Some("launcher") => settings::Page::Launcher,
            Some("about") => settings::Page::About,
            _ => settings::Page::General,
        };
        let out = args.get(i + 2).cloned().unwrap_or_else(|| "settings.bmp".into());
        let cfg = config::Config::load();
        let status = settings::Status {
            hotkey_ok: true,
            skins_running: 1,
            skin_notes: vec![(r"Summit\Time\Time.ini".to_string(), String::new())],
            memory_mb: 12.3,
            autostart: autostart::enabled(),
            apps: 342,
            taskbar: "Connected · clear (desktop)".into(),
            translucenttb: taskbar::translucenttb_running(),
            moving_skin: String::new(),
            skins_dir: rainmeter::Install::locate().skins.map(|p| p.display().to_string()).unwrap_or_default(),
        };
        std::process::exit(match settings::snapshot(&cfg, status, page) {
            Some((w, h, px)) => write_bmp(&out, w, h, &px),
            None => 2,
        });
    }
    // `lumen --restore`: undo the clear taskbar even if Lumen's windows won't open.
    if args.iter().any(|a| a == "--restore") {
        let mut cfg = config::Config::load();
        cfg.modules.taskbar = false;
        cfg.save();
        taskbar::restore_windows_look();
        unsafe {
            if let Ok(hwnd) = FindWindowW(w!("LumenApp"), None) {
                let _ = PostMessageW(hwnd, app::WM_RESTORE_TASKBAR, WPARAM(0), LPARAM(0));
            }
        }
        if args.iter().any(|a| a == "--restart-explorer") {
            taskbar::restart_explorer();
        }
        println!("Taskbar restored to normal and the clear taskbar turned off.");
        return;
    }
    // `lumen --uninstall`: undo everything Lumen changed in Windows before removing it.
    if args.iter().any(|a| a == "--uninstall") {
        let mut cfg = config::Config::load();
        cfg.modules.taskbar = false;
        cfg.save();
        taskbar::restore_windows_look();
        autostart::set(false);
        unsafe {
            if let Ok(hwnd) = FindWindowW(w!("LumenApp"), None) {
                let _ = PostMessageW(hwnd, app::WM_RESTORE_TASKBAR, WPARAM(0), LPARAM(0));
                let _ = PostMessageW(hwnd, windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if taskbar::helper_loaded() {
            taskbar::restart_explorer();
            std::thread::sleep(std::time::Duration::from_secs(4));
        }
        taskbar::remove_helper_files();
        println!("Lumen's changes are undone: normal taskbar, no autostart, helper removed from Explorer.");
        println!("Your settings are in %APPDATA%\\Lumen and logs in %LOCALAPPDATA%\\Lumen if you want to delete them too.");
        return;
    }
    let open_launcher = args.iter().any(|a| a == "--launcher");
    let open_settings = args.iter().any(|a| a == "--settings");

    unsafe {
        let _mutex = match CreateMutexW(None, true, w!("Local\\Lumen.App")) {
            Ok(m) => m,
            Err(e) => {
                error!("could not create single-instance mutex: {e}");
                return;
            }
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // `lumen --launcher` while Lumen runs: ask the running copy to open it.
            // A plain second launch (double-clicking Lumen again) opens settings.
            if let Ok(hwnd) = FindWindowW(w!("LumenApp"), None) {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                let msg = if open_launcher { app::WM_OPEN_LAUNCHER } else { app::WM_OPEN_SETTINGS };
                let _ = PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0));
            }
            return;
        }
        info!("Lumen {} starting", env!("CARGO_PKG_VERSION"));
        if let Err(e) = app::run(open_settings) {
            error!("app stopped: {e}");
        }
        info!("Lumen exited");
    }
}

/// Hands a notification to the running Lumen (exit 0), or reports that it isn't running.
fn send_notice(title: &str, text: &str, exe: &str, url: &str) -> i32 {
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_COPYDATA};
    let clean = |s: &str| s.replace('\u{1f}', " ");
    let payload = [clean(title), clean(text), String::new(), clean(exe), clean(url)].join("\u{1f}");
    if payload.len() >= 16_384 {
        eprintln!("Notification is too long.");
        return 1;
    }
    let cds = COPYDATASTRUCT { dwData: app::NOTIFY_MAGIC, cbData: payload.len() as u32, lpData: payload.as_ptr() as *mut _ };
    unsafe {
        let Ok(hwnd) = FindWindowW(w!("LumenApp"), None) else {
            eprintln!("Lumen is not running.");
            return 1;
        };
        let mut result = 0usize;
        let sent = SendMessageTimeoutW(hwnd, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const _ as isize), SMTO_ABORTIFHUNG, 2000, Some(&mut result));
        if sent.0 == 0 || result == 0 {
            eprintln!("Lumen did not take the notification. Is the Notifications module on?");
            return 1;
        }
    }
    0
}

fn launcher_snapshot(q: &str, out: &str) -> i32 {
    let mut cfg = config::Config::load();
    // LUMEN_SNAP_CLEAN: no file, folder or window results, so screenshots never show personal file names.
    if std::env::var_os("LUMEN_SNAP_CLEAN").is_some() {
        for id in ["files", "folder", "windows"] {
            if let Some(p) = cfg.launcher.plugins.get_mut(id) {
                p.enabled = false;
            }
        }
    }
    let Ok(mut l) = launcher::Launcher::create(&cfg.launcher, &cfg.appearance) else { return 1 };
    let Some((w, h, px)) = l.snapshot(q) else { return 2 };
    write_bmp(out, w, h, &px)
}

/// Writes the clock frame as a 32-bit BMP over a dark background.
fn snapshot(out: &str) -> i32 {
    let cfg = config::Config::load();
    let Some(first) = cfg.skins.iter().find(|s| s.enabled) else { return 1 };
    let Ok(mut clock) = clock::Clock::create(first, &cfg.clock.skins_path) else { return 1 };
    let Some((w, h, px)) = clock.snapshot() else { return 2 };
    write_bmp(out, w, h, &px)
}

fn write_bmp(out: &str, w: u32, h: u32, px: &[u8]) -> i32 {
    let mut bmp = Vec::with_capacity(54 + px.len());
    let size = 54 + px.len() as u32;
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(w as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(h as i32)).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    // Composite premultiplied pixels over a dark grey so the white text is visible.
    let bg: f32 = std::env::var("LUMEN_SNAP_BG").ok().and_then(|v| v.parse().ok()).unwrap_or(40.0);
    // LUMEN_SNAP_ALPHA keeps the premultiplied pixels and their alpha untouched (for converting to a transparent PNG).
    let keep = std::env::var_os("LUMEN_SNAP_ALPHA").is_some();
    for p in px.chunks_exact(4) {
        if keep {
            bmp.extend_from_slice(p);
            continue;
        }
        let a = p[3] as f32 / 255.0;
        for c in &p[..3] {
            bmp.push((*c as f32 + bg * (1.0 - a)).min(255.0) as u8);
        }
        bmp.push(255);
    }
    match std::fs::write(out, bmp) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}
