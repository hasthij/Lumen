//! Slow sources, run on a worker thread so typing never stalls:
//! - Folder: browse a typed path ("C:\Users\", "%appdata%\", "~\Downloads", "\\server\share")
//! - Files: Windows Search index when the service is running; otherwise a quick,
//!   time-limited scan of the user's folders (no index kept in memory)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::launcher::fuzzy;
use crate::launcher::item::{file_actions, Action, Icon, Item, Kind};
use crate::launcher::shell::expand_env;

pub enum Job {
    Folder(String),
    Files(String),
}

pub struct Worker {
    tx: Sender<(u64, Vec<Job>)>,
    latest: Arc<AtomicU64>,
    pub results: Arc<Mutex<(u64, Vec<Item>)>>,
}

impl Worker {
    pub fn start(notify: HWND, msg: u32) -> Self {
        let (tx, rx) = channel::<(u64, Vec<Job>)>();
        let latest = Arc::new(AtomicU64::new(0));
        let results = Arc::new(Mutex::new((0, Vec::new())));
        let (l, r) = (latest.clone(), results.clone());
        let notify = notify.0 as isize;
        let _ = std::thread::Builder::new().name("lumen-search".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            run(rx, l, r, HWND(notify as *mut _), msg);
        });
        Self { tx, latest, results }
    }

    /// Queues work for query generation `gen`; older pending work is dropped.
    pub fn submit(&self, gen: u64, jobs: Vec<Job>) {
        self.latest.store(gen, Ordering::SeqCst);
        if !jobs.is_empty() {
            let _ = self.tx.send((gen, jobs));
        }
    }
}

fn run(rx: Receiver<(u64, Vec<Job>)>, latest: Arc<AtomicU64>, results: Arc<Mutex<(u64, Vec<Item>)>>, notify: HWND, msg: u32) {
    while let Ok(mut next) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            next = newer;
        }
        let (gen, jobs) = next;
        let mut items = Vec::new();
        for job in jobs {
            if latest.load(Ordering::SeqCst) != gen {
                break;
            }
            match job {
                Job::Folder(q) => folder(&q, &mut items),
                Job::Files(q) => files(&q, &latest, gen, &mut items),
            }
        }
        if latest.load(Ordering::SeqCst) == gen {
            if let Ok(mut r) = results.lock() {
                *r = (gen, items);
            }
            unsafe {
                let _ = PostMessageW(notify, msg, WPARAM(gen as usize), LPARAM(0));
            }
        }
    }
}

/// Does this look like a path the user is typing?
pub fn is_path(q: &str) -> bool {
    let b = q.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || q.starts_with("\\\\") || q.starts_with('%') || q.starts_with("~\\") || q == "~"
}

fn folder(q: &str, out: &mut Vec<Item>) {
    let mut path = expand_env(q.trim());
    if let Some(rest) = path.strip_prefix('~') {
        path = format!("{}{rest}", std::env::var("USERPROFILE").unwrap_or_default());
    }
    if path.len() == 2 && path.ends_with(':') {
        path.push('\\');
    }
    let path = path.replace('/', "\\");
    let (dir, filter) = match path.rfind('\\') {
        Some(i) => (&path[..=i], path[i + 1..].to_lowercase()),
        None => return,
    };
    let dir_path = PathBuf::from(dir);
    let Ok(entries) = std::fs::read_dir(&dir_path) else { return };

    if filter.is_empty() {
        out.push(file_actions(
            Item::new(Kind::Folder, dir.trim_end_matches('\\').to_string() + "\\", "Open folder", Action::open(dir))
                .icon(Icon::Shell(dir.to_string()))
                .key(format!("folder:{dir}"))
                .score(6000),
            dir,
            true,
            false,
        ));
    }
    let mut found: Vec<(i32, bool, String)> = Vec::new();
    for e in entries.flatten().take(5000) {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let s = if filter.is_empty() { Some(0) } else { fuzzy::score(&filter, &name) };
        if let Some(s) = s {
            found.push((s + if is_dir { 50 } else { 0 }, is_dir, name));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.to_lowercase().cmp(&b.2.to_lowercase())));
    for (s, is_dir, name) in found.into_iter().take(40) {
        let full = dir_path.join(&name).display().to_string();
        let kind = if is_dir { Kind::Folder } else { Kind::File };
        let item = Item::new(kind, name, crate::launcher::item::folder_label(&full), Action::open(full.clone()))
            .icon(Icon::Shell(full.clone()))
            .key(format!("path:{full}"))
            .score(5000 + s);
        out.push(file_actions(item, &full, is_dir, !is_dir && is_runnable(&full)));
    }
}

pub fn is_runnable(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("exe" | "bat" | "cmd" | "msi" | "ps1" | "lnk" | "com")
    )
}

fn files(q: &str, latest: &AtomicU64, gen: u64, out: &mut Vec<Item>) {
    let q = q.trim();
    if q.chars().count() < 2 {
        return;
    }
    if search_service_running() {
        indexed(q, latest, gen, out);
    } else {
        quick_scan(q, latest, gen, out);
    }
}

fn search_service_running() -> bool {
    use windows::core::w;
    use windows::Win32::System::Services::*;
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else { return false };
        let running = OpenServiceW(scm, w!("WSearch"), SERVICE_QUERY_STATUS).is_ok_and(|svc| {
            let mut st = SERVICE_STATUS::default();
            let ok = QueryServiceStatus(svc, &mut st).is_ok() && st.dwCurrentState == SERVICE_RUNNING;
            let _ = CloseServiceHandle(svc);
            ok
        });
        let _ = CloseServiceHandle(scm);
        running
    }
}

/// Breadth-first name search of the user's folders, shallow files first, stopped after
/// 400 ms or when a newer query arrives. Listing names never downloads OneDrive files.
fn quick_scan(q: &str, latest: &AtomicU64, gen: u64, out: &mut Vec<Item>) {
    const SKIP: &[&str] = &["node_modules", ".git", "$recycle.bin", "appdata", "__pycache__", "target", ".cache"];
    let home = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default());
    let mut queue: std::collections::VecDeque<PathBuf> =
        ["Desktop", "Downloads", "Documents", "Pictures", "Videos", "Music", "OneDrive"].iter().map(|d| home.join(d)).filter(|d| d.is_dir()).collect();
    let ql = q.to_lowercase();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
    let mut found: Vec<(i32, PathBuf, bool)> = Vec::new();
    let mut depth_marker = queue.len();
    let mut depth = 0;
    while let Some(dir) = queue.pop_front() {
        if std::time::Instant::now() > deadline || latest.load(Ordering::SeqCst) != gen || found.len() >= 40 {
            break;
        }
        depth_marker = depth_marker.saturating_sub(1);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.len() > 1 {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // Substring matches only: a loose subsequence over thousands of files is noise.
            if let Some(s) = fuzzy::score(&ql, &name).filter(|&s| s >= 400) {
                found.push((s - depth * 30, e.path(), is_dir));
            }
            if is_dir && !SKIP.contains(&name.to_lowercase().as_str()) {
                queue.push_back(e.path());
            }
        }
        if depth_marker == 0 {
            depth += 1;
            depth_marker = queue.len();
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    for (i, (s, path, is_dir)) in found.into_iter().take(15).enumerate() {
        let p = path.display().to_string();
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone());
        let it = Item::new(if is_dir { Kind::Folder } else { Kind::File }, title, crate::launcher::item::folder_label(&p), Action::open(p.clone()))
            .icon(Icon::Shell(p.clone()))
            .key(format!("path:{p}"))
            .score(s / 2 - i as i32);
        out.push(file_actions(it, &p, is_dir, !is_dir && is_runnable(&p)));
    }
}

/// Windows Search via a `search-ms:` shell folder: enumerating it runs an indexed query.
fn indexed(q: &str, latest: &AtomicU64, gen: u64, out: &mut Vec<Item>) {
    let encoded: String = q
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    let url = format!("search-ms:query={encoded}&crumb=location:{}", std::env::var("USERPROFILE").unwrap_or_default());
    unsafe {
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(url), None) else { return };
        let Ok(items) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return };
        let ql = q.to_lowercase();
        let mut count = 0;
        loop {
            if latest.load(Ordering::SeqCst) != gen || count >= 15 {
                break;
            }
            let mut batch: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0;
            if items.Next(&mut batch, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = batch[0].take() else { break };
            let Some(path) = name(&item, SIGDN_FILESYSPATH) else { continue };
            let title = name(&item, SIGDN_NORMALDISPLAY).unwrap_or_else(|| path.clone());
            let is_dir = Path::new(&path).is_dir();
            let s = fuzzy::score(&ql, &title).unwrap_or(0);
            let it = Item::new(if is_dir { Kind::Folder } else { Kind::File }, title, crate::launcher::item::folder_label(&path), Action::open(path.clone()))
                .icon(Icon::Shell(path.clone()))
                .key(format!("path:{path}"))
                // Files rank under apps and exact commands but above loose matches.
                .score(s / 2 - count);
            out.push(file_actions(it, &path, is_dir, !is_dir && is_runnable(&path)));
            count += 1;
        }
    }
}

unsafe fn name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    let p: PWSTR = item.GetDisplayName(sigdn).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}
