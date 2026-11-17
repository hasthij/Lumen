//! Now-playing info and controls through Windows' media session API (SMTC), for every app
//! that has a session. One worker thread lives while the island does; it sleeps unless the panel is open.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

#[derive(Clone)]
pub struct Media {
    /// The app's id; commands are addressed with it.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// Position in seconds at `at`.
    pub pos: f64,
    pub dur: f64,
    pub at: Instant,
    /// Encoded image bytes; `art_gen` changes whenever they do.
    pub art: Option<Arc<Vec<u8>>>,
    pub art_gen: u32,
}

impl Media {
    /// Position now, extrapolated while playing.
    pub fn position(&self) -> f64 {
        let p = if self.playing { self.pos + self.at.elapsed().as_secs_f64() } else { self.pos };
        if self.dur > 0.0 { p.clamp(0.0, self.dur) } else { p.max(0.0) }
    }

    fn same(&self, o: &Media) -> bool {
        self.id == o.id
            && self.title == o.title
            && self.artist == o.artist
            && self.playing == o.playing
            && self.dur == o.dur
            && self.art_gen == o.art_gen
            // Position drifts by itself while playing; only a jump counts as a change.
            && (self.position() - o.position()).abs() <= 1.5
    }
}

/// Commands name the session (app id) they act on.
pub enum Cmd {
    Refresh,
    Toggle(String),
    Next(String),
    Prev(String),
    Seek(String, f64),
    Quit,
}

impl Cmd {
    fn id(&self) -> Option<String> {
        match self {
            Cmd::Toggle(i) | Cmd::Next(i) | Cmd::Prev(i) | Cmd::Seek(i, _) => Some(i.clone()),
            _ => None,
        }
    }
}

pub struct Worker {
    pub tx: Sender<Cmd>,
    /// Sessions in the order they first appeared: the first one is the "main" one.
    pub state: Arc<Mutex<Vec<Media>>>,
    pub active: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    /// `hwnd` receives `msg` whenever the state changes.
    pub fn start(hwnd: HWND, msg: u32) -> Worker {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicBool::new(false));
        let (s, a) = (state.clone(), active.clone());
        let h = hwnd.0 as usize;
        let handle = std::thread::spawn(move || run(rx, s, a, h, msg));
        Worker { tx, state, active, handle: Some(handle) }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Detached on purpose: the worker can sit in a blocking WinRT call, and joining would freeze the UI.
        let _ = self.tx.send(Cmd::Quit);
        self.handle.take();
    }
}

fn run(rx: mpsc::Receiver<Cmd>, state: Arc<Mutex<Vec<Media>>>, active: Arc<AtomicBool>, hwnd: usize, msg: u32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // Every COM object lives inside `run_inner`, so all are released before CoUninitialize.
    run_inner(rx, state, active, hwnd, msg);
    unsafe { CoUninitialize() };
}

/// Per-app art cache so a track's cover is fetched once.
#[derive(Default)]
struct Slot {
    id: String,
    key: String,
    art: Option<Arc<Vec<u8>>>,
    gen: u32,
    tries: u32,
}

fn run_inner(rx: mpsc::Receiver<Cmd>, state: Arc<Mutex<Vec<Media>>>, active: Arc<AtomicBool>, hwnd: usize, msg: u32) {
    let mut mgr = Manager::RequestAsync().and_then(|o| o.get()).ok();
    let notify = || unsafe {
        let _ = PostMessageW(HWND(hwnd as *mut _), msg, WPARAM(0), LPARAM(0));
    };
    let mut slots: Vec<Slot> = Vec::new();
    let mut last_n = usize::MAX;
    let mut last_track: Option<(String, String)> = None;
    let mut last_playing;
    let mut last_primary = String::new();
    let mut sp_pid = 0u32;
    // Cover for the Spotify stand-in, looked up on a helper thread once per track.
    let fb_art: Arc<Mutex<Option<(String, Arc<Vec<u8>>)>>> = Arc::new(Mutex::new(None));
    let mut fb_key = String::new();
    let mut fb_gen = 0u32;
    let mut fb_have = false;
    loop {
        let cmd = if active.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break,
            }
        };
        if mgr.is_none() {
            mgr = Manager::RequestAsync().and_then(|o| o.get()).ok();
        }
        let mut sessions = mgr.as_ref().map(list).unwrap_or_default();
        if sessions.is_empty() {
            // A manager made before any app registered can stay empty; make a fresh one, then ask for the current session.
            mgr = Manager::RequestAsync().and_then(|o| o.get()).ok();
            if let Some(m) = &mgr {
                sessions = list(m);
                if sessions.is_empty() {
                    if let Ok(s) = m.GetCurrentSession() {
                        let id = s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default();
                        sessions.push((id, s));
                    }
                }
            }
        }
        if sessions.len() != last_n {
            crate::info!("island media: manager={} sessions={:?}", mgr.is_some(), sessions.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>());
            last_n = sessions.len();
        }
        let cmd_was_some = cmd.is_some();
        match cmd {
            Some(Cmd::Quit) => break,
            Some(Cmd::Refresh) | None => {}
            Some(other) => {
                let id = other.id();
                match sessions.iter().find(|(i, _)| Some(i) == id.as_ref()) {
                    Some((_, s)) => {
                        if let Err(e) = control(s, other) {
                            crate::warn!("island media command failed: {e}");
                        }
                    }
                    None if id.as_deref() == Some(FALLBACK) => media_key(&other),
                    None => crate::warn!("island media command: session {:?} not found", id),
                }
            }
        }
        // Keep first-seen order; drop apps that left, append new ones.
        slots.retain(|sl| sessions.iter().any(|(i, _)| *i == sl.id));
        for (i, _) in &sessions {
            if !slots.iter().any(|sl| sl.id == *i) {
                slots.push(Slot { id: i.clone(), ..Default::default() });
            }
        }
        let mut fresh = Vec::new();
        for sl in slots.iter_mut() {
            if let Some((_, s)) = sessions.iter().find(|(i, _)| *i == sl.id) {
                if let Some(m) = read(s, sl) {
                    fresh.push(m);
                }
            }
        }
        // Spotify often registers no Windows media session; read its window unless it has one.
        if !sessions.iter().any(|(i, _)| i.to_ascii_lowercase().contains("spotify")) {
            if let Some(t) = spotify_title(&mut sp_pid) {
                if let Some(tr) = t {
                    last_track = Some(tr);
                    last_playing = true;
                } else {
                    last_playing = false;
                }
                if let Some((title, artist)) = last_track.clone() {
                    let key = format!("{artist}\u{1}{title}");
                    if key != fb_key {
                        fb_key = key.clone();
                        fb_have = false;
                        fb_gen += 1;
                        let slot = fb_art.clone();
                        let q = format!("{artist} {title}");
                        std::thread::spawn(move || {
                            if let Some(bytes) = fetch_cover(&q) {
                                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, Arc::new(bytes)));
                            }
                        });
                    }
                    let art = match fb_art.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        Some((k, b)) if *k == fb_key => Some(b.clone()),
                        _ => None,
                    };
                    if art.is_some() && !fb_have {
                        fb_have = true;
                        fb_gen += 1;
                    }
                    fresh.push(Media { id: FALLBACK.into(), title, artist, playing: last_playing, pos: 0.0, dur: 0.0, at: Instant::now(), art, art_gen: fb_gen });
                }
            }
        }
        // Playing first; when nothing plays, the source that played last stays on top.
        fresh.sort_by_key(|m| (!m.playing, m.id != last_primary));
        if let Some(m) = fresh.first().filter(|m| m.playing) {
            last_primary.clone_from(&m.id);
        }
        let changed = {
            let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
            let same = m.len() == fresh.len() && m.iter().zip(&fresh).all(|(a, b)| a.same(b));
            if !same {
                *m = fresh;
            }
            !same
        };
        if changed || cmd_was_some {
            notify();
        }
    }
}

/// Looks a cover up on the iTunes Search API (no key) with the system curl. Only the track text
/// leaves the machine. Blocking: call from a helper thread.
fn fetch_cover(query: &str) -> Option<Vec<u8>> {
    use std::os::windows::process::CommandExt;
    let run = |args: &[&str]| -> Option<Vec<u8>> {
        let out = std::process::Command::new("curl.exe").args(args).creation_flags(0x0800_0000).output().ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    };
    let term = format!("term={query}");
    let json = run(&["-s", "-m", "8", "-G", "https://itunes.apple.com/search", "--data-urlencode", &term, "-d", "entity=song", "-d", "limit=1"])?;
    let json = String::from_utf8_lossy(&json);
    let at = json.find("\"artworkUrl100\"")? + "\"artworkUrl100\"".len();
    let rest = &json[at..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    let url = rest[start..end].replace("100x100", "600x600");
    if !url.starts_with("https://") {
        return None;
    }
    let bytes = run(&["-s", "-m", "10", &url])?;
    crate::info!("island media: cover fetched ({} bytes)", bytes.len());
    Some(bytes)
}

/// Readable app name for a session id (AUMID or exe path). Browsers registered under a random
/// id (e.g. "2PXGTUECDQ...") show as "Browser".
pub fn app_name(id: &str) -> String {
    let l = id.to_ascii_lowercase();
    for (k, n) in [("spotify", "Spotify"), ("brave", "Brave"), ("chrome", "Chrome"), ("msedge", "Edge"), ("firefox", "Firefox"), ("opera", "Opera"), ("vivaldi", "Vivaldi"), ("zunemusic", "Media Player"), ("vlc", "VLC"), ("discord", "Discord")] {
        if l.contains(k) {
            return n.into();
        }
    }
    let last = id.rsplit(['!', '\\', '.']).find(|p| !p.is_empty() && !p.eq_ignore_ascii_case("exe")).unwrap_or("");
    let random = last.len() >= 16 && last.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if last.is_empty() {
        "Media".into()
    } else if random {
        "Browser".into()
    } else {
        last.into()
    }
}

/// Id of the stand-in session used when Windows lists no media session but Spotify is running.
const FALLBACK: &str = "spotify:window";

/// Spotify's window title is "Artist - Song" while playing and "Spotify ..." otherwise.
/// Returns (title, artist) when playing, Some(None) when Spotify runs but is idle, None when it does not run.
/// `pid` caches Spotify's process so later ticks skip opening every window's process.
fn spotify_title(pid: &mut u32) -> Option<Option<(String, String)>> {
    if *pid != 0 {
        if let r @ Some(_) = scan_impl(*pid, &mut 0) {
            return r;
        }
    }
    *pid = 0;
    scan_impl(0, pid)
}

/// `want` != 0: only windows of that process. Otherwise matches by exe name and stores the pid in `found_pid`.
fn scan_impl(want: u32, found_pid: &mut u32) -> Option<Option<(String, String)>> {
    use windows::Win32::Foundation::{CloseHandle, BOOL, TRUE};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId};
    struct Acc {
        want: u32,
        pid: u32,
        running: bool,
        found: Option<String>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let acc = &mut *(lp.0 as *mut Acc);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if acc.want != 0 {
            if pid != acc.want {
                return TRUE;
            }
        } else {
            // Cheap title check first: Spotify's top windows always have a title.
            if GetWindowTextLengthW(h) == 0 {
                return TRUE;
            }
            let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return TRUE };
            let mut buf = [0u16; 520];
            let mut n = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
            let _ = CloseHandle(p);
            if !ok || !String::from_utf16_lossy(&buf[..n as usize]).to_ascii_lowercase().ends_with("\\spotify.exe") {
                return TRUE;
            }
        }
        let mut t = [0u16; 512];
        let len = GetWindowTextW(h, &mut t).max(0) as usize;
        let title = String::from_utf16_lossy(&t[..len]);
        if !title.is_empty() {
            acc.running = true;
            acc.pid = pid;
            if title.contains(" - ") && acc.found.is_none() {
                acc.found = Some(title);
            }
        }
        TRUE
    }
    let mut acc = Acc { want, pid: 0, running: false, found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut acc as *mut Acc as isize));
    }
    if !acc.running {
        return None;
    }
    *found_pid = acc.pid;
    Some(acc.found.and_then(|t| t.split_once(" - ").map(|(a, s)| (s.trim().to_string(), a.trim().to_string()))))
}

/// Global media key: Spotify (and most players) react to it even without a Windows media session.
fn media_key(c: &Cmd) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK};
    let vk = match c {
        Cmd::Toggle(_) => VK_MEDIA_PLAY_PAUSE,
        Cmd::Next(_) => VK_MEDIA_NEXT_TRACK,
        Cmd::Prev(_) => VK_MEDIA_PREV_TRACK,
        _ => return,
    };
    unsafe {
        keybd_event(vk.0 as u8, 0, Default::default(), 0);
        keybd_event(vk.0 as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

fn control(s: &Session, c: Cmd) -> windows::core::Result<bool> {
    match c {
        // Explicit play/pause: many apps ignore the toggle command.
        Cmd::Toggle(_) => {
            let playing = s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|p| p == Status::Playing).unwrap_or(false);
            if playing { s.TryPauseAsync() } else { s.TryPlayAsync() }.and_then(|o| o.get())
        }
        Cmd::Next(_) => s.TrySkipNextAsync().and_then(|o| o.get()),
        Cmd::Prev(_) => s.TrySkipPreviousAsync().and_then(|o| o.get()),
        Cmd::Seek(_, secs) => {
            let start = s.GetTimelineProperties().and_then(|t| t.StartTime()).map(|d| d.Duration).unwrap_or(0);
            s.TryChangePlaybackPositionAsync(start + (secs * 1e7) as i64).and_then(|o| o.get())
        }
        _ => Ok(false),
    }
}

fn list(m: &Manager) -> Vec<(String, Session)> {
    let mut out = Vec::new();
    if let Ok(l) = m.GetSessions() {
        for s in l {
            let id = s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default();
            out.push((id, s));
        }
    }
    out
}

/// Reads one session; None when it has no title/artist (nothing to show).
fn read(s: &Session, slot: &mut Slot) -> Option<Media> {
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let mut title = props.Title().map(|t| t.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|a| a.to_string()).unwrap_or_default();
    if title.is_empty() && artist.is_empty() {
        // Still show the app so the panel binds to it and its controls work.
        title = app_name(&slot.id);
    }
    let playing = s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).map(|p| p == Status::Playing).unwrap_or(false);
    let (mut pos, mut dur) = (0.0, 0.0);
    if let Ok(t) = s.GetTimelineProperties() {
        let start = t.StartTime().map(|d| d.Duration).unwrap_or(0);
        let end = t.EndTime().map(|d| d.Duration).unwrap_or(0);
        pos = (t.Position().map(|d| d.Duration).unwrap_or(0) - start) as f64 / 1e7;
        dur = (end - start) as f64 / 1e7;
        if playing {
            // SMTC only refreshes the position on events; add the time since it did.
            if let Ok(u) = t.LastUpdatedTime() {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let last = u.UniversalTime as f64 / 1e7 - 11_644_473_600.0;
                if now > last && now - last < 86_400.0 {
                    pos += now - last;
                }
            }
        }
    }
    // Art often shows up after the title; retry a few times while it is missing.
    let key = format!("{title}\u{1}{artist}");
    let new_track = slot.key != key;
    if new_track || (slot.art.is_none() && slot.tries < 6) {
        if new_track {
            // Never keep the previous track's cover while the new one loads.
            slot.key = key;
            slot.tries = 0;
            slot.art = None;
            slot.gen = slot.gen.wrapping_add(1);
        }
        let art = thumbnail(&props).map(Arc::new);
        slot.tries = if art.is_some() { 99 } else { slot.tries + 1 };
        if art.is_some() {
            slot.art = art;
            slot.gen = slot.gen.wrapping_add(1);
        }
    }
    Some(Media { id: slot.id.clone(), title, artist, playing, pos, dur, at: Instant::now(), art: slot.art.clone(), art_gen: slot.gen })
}

fn thumbnail(props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties) -> Option<Vec<u8>> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as usize;
    if size == 0 || size > 8 << 20 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    let n = (reader.LoadAsync(size as u32).ok()?.get().ok()? as usize).min(size);
    if n == 0 {
        return None;
    }
    let mut buf = vec![0u8; n];
    reader.ReadBytes(&mut buf).ok()?;
    Some(buf)
}
