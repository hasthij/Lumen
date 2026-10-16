//! Installed-app index with live updates.
//!
//! Sources: the shell's AppsFolder (every Start menu app, Win32 and Store alike) plus
//! shortcuts on the desktop. A background thread rescans when the Start menu or desktop
//! folders change on disk, or when asked to (the launcher asks on open if the index is
//! older than 30 s, which catches Store installs). New apps therefore appear without
//! restarting anything.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Storage::EnhancedStorage::PKEY_Link_TargetParsingPath;
use windows::Win32::Storage::FileSystem::{
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::shell::APPS_FOLDER;

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    /// What to launch: `shell:AppsFolder\<id>` or a shortcut path.
    pub launch: String,
    /// Real file behind the app, when there is one (for admin / show in folder).
    pub path: Option<String>,
    pub packaged: bool,
}

pub struct Index {
    apps: Mutex<Arc<Vec<App>>>,
    scanned: Mutex<Option<Instant>>,
    wake: HANDLE,
    stop: HANDLE,
}

// HANDLEs are plain kernel handles, safe to use from any thread.
unsafe impl Send for Index {}
unsafe impl Sync for Index {}

impl Index {
    /// Starts the index thread. `notify` gets `msg` posted after every rescan.
    pub fn start(notify: HWND, msg: u32) -> Arc<Self> {
        let index = Arc::new(Self {
            apps: Mutex::new(Arc::new(Vec::new())),
            scanned: Mutex::new(None),
            wake: unsafe { CreateEventW(None, false, true, None).unwrap_or_default() },
            stop: unsafe { CreateEventW(None, true, false, None).unwrap_or_default() },
        });
        let worker = index.clone();
        let notify = notify.0 as isize;
        let spawned = std::thread::Builder::new().name("lumen-app-index".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            worker.run(HWND(notify as *mut _), msg);
        });
        if let Err(e) = spawned {
            crate::error!("could not start app index thread: {e}");
        }
        index
    }

    pub fn apps(&self) -> Arc<Vec<App>> {
        self.apps.lock().map(|a| a.clone()).unwrap_or_default()
    }

    /// Rescans in the background if the index is older than `max_age`.
    pub fn refresh_if_older(&self, max_age: Duration) {
        let stale = self.scanned.lock().map(|t| t.map_or(true, |t| t.elapsed() > max_age)).unwrap_or(true);
        if stale {
            unsafe {
                let _ = SetEvent(self.wake);
            }
        }
    }

    pub fn shutdown(&self) {
        unsafe {
            let _ = SetEvent(self.stop);
        }
    }

    fn run(&self, notify: HWND, msg: u32) {
        let watched = watch_dirs();
        let mut handles = vec![self.stop, self.wake];
        let mut changes = Vec::new();
        for dir in &watched {
            let h = unsafe {
                FindFirstChangeNotificationW(
                    &HSTRING::from(dir.as_str()),
                    true,
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                )
            };
            match h {
                Ok(h) if !h.is_invalid() => {
                    handles.push(h);
                    changes.push(h);
                }
                _ => crate::warn!("cannot watch {dir} for new apps"),
            }
        }

        loop {
            let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            let i = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if i == 0 || i >= handles.len() {
                break;
            }
            if i >= 2 {
                // Installers write many files; wait for things to settle before rescanning.
                unsafe {
                    let _ = FindNextChangeNotification(handles[i]);
                }
                std::thread::sleep(Duration::from_millis(1500));
                for &h in &changes {
                    unsafe {
                        if WaitForMultipleObjects(&[h], false, 0) == WAIT_OBJECT_0 {
                            let _ = FindNextChangeNotification(h);
                        }
                    }
                }
            }
            let started = Instant::now();
            let apps = scan();
            let changed = self.apps.lock().map(|a| !same(&a, &apps)).unwrap_or(true);
            if let Ok(mut s) = self.scanned.lock() {
                *s = Some(Instant::now());
            }
            if changed {
                crate::info!("app index: {} apps ({} ms)", apps.len(), started.elapsed().as_millis());
                if let Ok(mut a) = self.apps.lock() {
                    *a = Arc::new(apps);
                }
                unsafe {
                    let _ = PostMessageW(notify, msg, WPARAM(0), LPARAM(0));
                }
            }
        }
        for h in changes {
            unsafe {
                let _ = FindCloseChangeNotification(h);
            }
        }
        unsafe {
            let _ = CloseHandle(self.wake);
            let _ = CloseHandle(self.stop);
        }
    }
}

fn same(a: &[App], b: &[App]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.launch == y.launch && x.name == y.name)
}

fn watch_dirs() -> Vec<String> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    [
        format!(r"{}\Microsoft\Windows\Start Menu\Programs", env("APPDATA")),
        format!(r"{}\Microsoft\Windows\Start Menu\Programs", env("PROGRAMDATA")),
        format!(r"{}\Desktop", env("USERPROFILE")),
        format!(r"{}\Desktop", env("PUBLIC")),
    ]
    .into_iter()
    .filter(|d| std::path::Path::new(d).is_dir())
    .collect()
}

fn scan() -> Vec<App> {
    let mut apps = scan_apps_folder();
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    for desktop in [format!(r"{}\Desktop", env("USERPROFILE")), format!(r"{}\Desktop", env("PUBLIC"))] {
        let Ok(entries) = std::fs::read_dir(&desktop) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
            if !matches!(ext.as_deref(), Some("lnk" | "url" | "appref-ms")) {
                continue;
            }
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if apps.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) {
                continue;
            }
            let launch = p.display().to_string();
            apps.push(App { path: Some(launch.clone()), name, launch, packaged: false });
        }
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

fn scan_apps_folder() -> Vec<App> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<_, IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) else {
            crate::warn!("AppsFolder unavailable");
            return out;
        };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut batch: [Option<IShellItem>; 32] = Default::default();
            let mut fetched = 0;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            for item in batch.iter().take(fetched as usize).flatten() {
                if let Some(app) = app_from_item(item) {
                    out.push(app);
                }
            }
        }
    }
    out
}

unsafe fn app_from_item(item: &IShellItem) -> Option<App> {
    let name = display_name(item, SIGDN_NORMALDISPLAY)?;
    let id = display_name(item, SIGDN_PARENTRELATIVEPARSING)?;
    if name.is_empty() || id.is_empty() {
        return None;
    }
    // Win32 apps come from shortcuts; their target is the real exe.
    let link_target = item
        .cast::<IShellItem2>()
        .ok()
        .and_then(|i2| i2.GetString(&PKEY_Link_TargetParsingPath).ok())
        .map(|p| {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        })
        .filter(|s| !s.is_empty());
    let path = link_target.or_else(|| known_folder_path(&id)).filter(|p| std::path::Path::new(p).exists());
    let packaged = path.is_none() && id.contains('!');
    Some(App { name, launch: format!("{APPS_FOLDER}{id}"), path, packaged })
}

unsafe fn display_name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    let p: PWSTR = item.GetDisplayName(sigdn).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// `{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\notepad.exe` → `C:\Windows\System32\notepad.exe`.
fn known_folder_path(id: &str) -> Option<String> {
    let (guid, rest) = id.strip_prefix('{')?.split_once("}\\")?;
    let guid = windows::core::GUID::try_from(guid).ok()?;
    unsafe {
        let p = SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, None).ok()?;
        let base = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        Some(format!("{}\\{rest}", base?))
    }
}
