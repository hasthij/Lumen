//! Carrying out result actions, plus small shell helpers shared by plugins.

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ};
use windows::Win32::System::Shutdown::{ExitWindowsEx, LockWorkStation, EWX_LOGOFF, SHUTDOWN_REASON};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::item::{hwnd_of, Action, SystemCommand};

pub const APPS_FOLDER: &str = r"shell:AppsFolder\";

/// Runs an action. Returns false when it could not be started.
pub fn run(action: &Action, owner: HWND) -> bool {
    match action {
        // Launching can wait on a UAC prompt or a slow program. Doing that on the UI thread
        // would freeze Lumen (and drop the global shortcut) until it finishes, so it runs aside.
        Action::Open { target, args } => spawn_execute("open", target.clone(), args.clone()),
        Action::RunAsAdmin { target, args } | Action::Elevated { target, args, .. } => spawn_execute("runas", target.clone(), args.clone()),
        Action::ShowInFolder(path) => show_in_folder(path),
        Action::Copy(text) => copy(owner, text),
        Action::Terminal(dir) => terminal(dir),
        Action::SwitchTo(h) => switch_to(hwnd_of(*h)),
        Action::CloseWindow(h) => unsafe { PostMessageW(hwnd_of(*h), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() },
        Action::System(cmd) => system(*cmd),
        Action::RegistryOpen(key) => open_regedit(key),
    }
}

fn spawn_execute(verb: &'static str, target: String, args: String) -> bool {
    std::thread::Builder::new()
        .name("lumen-launch".into())
        .spawn(move || {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            if !execute(verb, &target, &args, None) {
                crate::warn!("action failed: {verb} {target}");
            }
        })
        .is_ok()
}

pub fn execute(verb: &str, target: &str, args: &str, dir: Option<&str>) -> bool {
    let verb = HSTRING::from(verb);
    let file = HSTRING::from(target);
    let params = HSTRING::from(args);
    let dir = dir.map(HSTRING::from);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: if args.is_empty() { PCWSTR::null() } else { PCWSTR(params.as_ptr()) },
        lpDirectory: dir.as_ref().map(|d| PCWSTR(d.as_ptr())).unwrap_or(PCWSTR::null()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => true,
        Err(e) => {
            // The user cancelling a UAC prompt is not an error worth logging loudly.
            if e.code().0 as u32 != 0x800704C7 {
                crate::warn!("could not open {target}: {e}");
            }
            false
        }
    }
}

/// Opens Explorer with the item selected, reusing an open Explorer window when possible.
pub fn show_in_folder(path: &str) -> bool {
    unsafe {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(&HSTRING::from(path), None, &mut pidl, 0, None).is_err() || pidl.is_null() {
            return execute("open", "explorer.exe", &format!("/select,\"{path}\""), None);
        }
        let ok = SHOpenFolderAndSelectItems(pidl, None, 0).is_ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(pidl as *const _));
        ok
    }
}

pub fn copy(owner: HWND, text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(owner).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = (|| {
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(mem);
            SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(mem.0)).ok()
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

/// Windows Terminal if installed, otherwise Command Prompt, started in `dir`.
pub fn terminal(dir: &str) -> bool {
    execute("open", "wt.exe", &format!("-d \"{dir}\""), None) || execute("open", "cmd.exe", "", Some(dir))
}

pub fn switch_to(hwnd: HWND) -> bool {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

fn system(cmd: SystemCommand) -> bool {
    unsafe {
        match cmd {
            SystemCommand::Lock => LockWorkStation().is_ok(),
            SystemCommand::Sleep => windows::Win32::System::Power::SetSuspendState(false, false, false).as_bool(),
            SystemCommand::Hibernate => windows::Win32::System::Power::SetSuspendState(true, false, false).as_bool(),
            SystemCommand::Shutdown => execute("open", "shutdown.exe", "/s /hybrid /t 0", None),
            SystemCommand::Restart => execute("open", "shutdown.exe", "/r /t 0", None),
            SystemCommand::SignOut => ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0)).is_ok(),
            // Flags 0: Windows shows its own "are you sure" dialog.
            SystemCommand::EmptyRecycleBin => SHEmptyRecycleBinW(None, None, 0).is_ok(),
        }
    }
}

/// Points Registry Editor at `key` (via its LastKey setting) and opens it.
fn open_regedit(key: &str) -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        let sub = w!(r"Software\Microsoft\Windows\CurrentVersion\Applets\Regedit");
        if RegCreateKeyExW(HKEY_CURRENT_USER, sub, 0, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut hkey, None).is_ok() {
            let value = format!("Computer\\{key}");
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let _ = RegSetValueExW(hkey, w!("LastKey"), 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(hkey);
        }
    }
    execute("open", "regedit.exe", "-m", None)
}

pub fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = HSTRING::from(s);
    let mut buf = vec![0u16; 2048];
    let n = unsafe { windows::Win32::System::Environment::ExpandEnvironmentStringsW(&src, Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n - 1])
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
