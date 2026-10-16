//! Lumen: an all-in-one Windows customization app.
//!
//! Modules: desktop clock (native Rainmeter skin renderer). Launcher and clear taskbar
//! come next.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clock;
mod config;
mod gfx;
mod icon;
#[macro_use]
mod log;
mod rainmeter;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

fn main() {
    log::install_panic_hook();
    let args: Vec<String> = std::env::args().collect();

    // `lumen --snapshot out.bmp` renders the clock once to a file and exits (for testing).
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "clock.bmp".into());
        std::process::exit(snapshot(&out));
    }

    unsafe {
        let _mutex = match CreateMutexW(None, true, w!("Local\\Lumen.App")) {
            Ok(m) => m,
            Err(e) => {
                error!("could not create single-instance mutex: {e}");
                return;
            }
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            info!("Lumen is already running");
            return;
        }
        info!("Lumen {} starting", env!("CARGO_PKG_VERSION"));
        if let Err(e) = app::run() {
            error!("app stopped: {e}");
        }
        info!("Lumen exited");
    }
}

/// Writes the clock frame as a 32-bit BMP over a dark background.
fn snapshot(out: &str) -> i32 {
    let cfg = config::Config::load();
    let Ok(mut clock) = clock::Clock::create(&cfg.clock) else { return 1 };
    let Some((w, h, px)) = clock.snapshot() else { return 2 };
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
    let bg = 40.0;
    for p in px.chunks_exact(4) {
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
