//! Tiny append-only log at %LOCALAPPDATA%\Lumen\lumen.log, rotated at 1 MB.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 1024 * 1024;

pub fn dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Lumen")
}

pub fn write(level: &str, msg: &str) {
    let _guard = LOCK.lock();
    let dir = dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("lumen.log");
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("lumen.old.log"));
    }
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {level} {msg}\r\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::write("INFO ", &format!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::write("WARN ", &format!($($t)*)) };
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::log::write("ERROR", &format!($($t)*)) };
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        write("PANIC", &format!("{info}\r\n{bt}"));
    }));
}
