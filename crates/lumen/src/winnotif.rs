//! Windows toasts for MacNotif: reads what apps like Telegram or Discord post to Windows and hands
//! it to the notification stack, and keeps Windows' own banners quiet while Lumen shows them.
//!
//! Banners are muted globally (`ToastEnabled = 0`); Windows still records the toasts, so they can
//! be read. The original value is kept in a small file so it comes back, even after a crash.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_APP};
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds};

use crate::macnotif::Notice;

/// Posted to the notification window when new toasts are waiting in the queue.
pub const WM_NOTE: u32 = WM_APP + 0x32;

static QUEUE: Mutex<Vec<Notice>> = Mutex::new(Vec::new());
static BLOCK: AtomicBool = AtomicBool::new(false);

/// How long after a toast the banner hider keeps its fast poll.
const HIDER_BURST: Duration = Duration::from_secs(4);

/// Hider poll interval: tight just after a toast arrives, relaxed otherwise.
/// Without toast events nothing announces a toast before its banner shows, so stay fast.
fn hider_sleep(since_activity: Duration, events: bool) -> Duration {
    Duration::from_millis(if !events || since_activity < HIDER_BURST { 6 } else { 40 })
}

/// True once the listener's toast-event registration succeeded.
static EVENTS: AtomicBool = AtomicBool::new(false);

static ACTIVITY: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Marks "a toast just arrived" for the hider thread.
fn note_activity() {
    *ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
}

fn last_activity() -> std::time::Instant {
    // Before any toast the hider is relaxed: report an instant far enough back.
    let now = std::time::Instant::now();
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| now.checked_sub(HIDER_BURST * 2).unwrap_or(now))
}

const SETTINGS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Notifications\\Settings";

pub fn drain() -> Vec<Notice> {
    std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Starts reading Windows toasts for `hwnd` until that window goes away.
pub fn start(hwnd: HWND, block: bool) {
    legacy_restore();
    set_blocked(block);
    let h = hwnd.0 as isize;
    // Windows' own banner is one long-lived CoreWindow titled "New notification". The shell shows it
    // per toast; keep hiding it while blocking is on (registry switches don't stop it on Win11).
    std::thread::spawn(move || unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindowVisible, ShowWindow, SW_HIDE};
        let mut cached = HWND::default();
        let mut tick = 0u32;
        loop {
            std::thread::sleep(hider_sleep(last_activity().elapsed(), EVENTS.load(Ordering::Relaxed)));
            if !IsWindow(HWND(h as *mut _)).as_bool() {
                break;
            }
            if !BLOCK.load(Ordering::Relaxed) {
                continue;
            }
            // Cache the banner window: FindWindowW walks every top-level window, too costly at 6 ms.
            if !IsWindow(cached).as_bool() {
                tick += 1;
                if tick % 8 != 0 {
                    continue;
                }
                cached = FindWindowW(w!("Windows.UI.Core.CoreWindow"), w!("New notification")).unwrap_or_default();
                if cached.0.is_null() {
                    continue;
                }
            }
            if IsWindowVisible(cached).as_bool() {
                let _ = ShowWindow(cached, SW_HIDE);
            }
        }
    });
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(listener) = UserNotificationListener::Current() else { return };
        let access = listener.RequestAccessAsync().and_then(|o| o.get());
        if !matches!(access, Ok(UserNotificationListenerAccessStatus::Allowed)) {
            // Needs Settings > Privacy & security > Notifications to allow it.
            return;
        }
        // Wake the moment Windows posts a toast (polling alone lets the banner flash first).
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let events = listener
            .NotificationChanged(&windows::Foundation::TypedEventHandler::new(move |_, _| {
                note_activity();
                let _ = tx.send(());
                Ok(())
            }))
            .is_ok();
        EVENTS.store(events, Ordering::Relaxed);
        let mut seen: HashSet<u32> = HashSet::new();
        let mut first = true;
        // Removal is delayed so Windows' own notification sound plays out instead of being clipped.
        let mut pending: Vec<(std::time::Instant, u32)> = Vec::new();
        loop {
            // Event-driven; the timeout is only a safety net (fast while removals are pending).
            let wait = if !pending.is_empty() { 100 } else if !events { if BLOCK.load(Ordering::Relaxed) { 120 } else { 500 } } else { 2000 };
            // A dead channel returns instantly; never let that turn this into a busy loop.
            // Event registration fails for unpackaged apps, so the channel is dead: poll instead.
            if rx.recv_timeout(Duration::from_millis(wait)) == Err(std::sync::mpsc::RecvTimeoutError::Disconnected) {
                std::thread::sleep(Duration::from_millis(wait));
            }
            let hw = HWND(h as *mut _);
            if !IsWindow(hw).as_bool() {
                break;
            }
            let Ok(list) = listener.GetNotificationsAsync(NotificationKinds::Toast).and_then(|o| o.get()) else { continue };
            let mut now = HashSet::new();
            let mut fresh = false;
            for n in list {
                let Ok(id) = n.Id() else { continue };
                now.insert(id);
                // Blocking mode owns the toast surface: clear the backlog (it also makes every poll slow).
                if first && BLOCK.load(Ordering::Relaxed) {
                    let _ = listener.RemoveNotification(id);
                }
                if first || seen.contains(&id) {
                    continue;
                }
                if let Some(note) = read(&n) {
                    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(note);
                    fresh = true;
                }
                note_activity();
                // The banner is hidden by the hider thread; the toast itself is cleared a bit later.
                if BLOCK.load(Ordering::Relaxed) {
                    pending.push((std::time::Instant::now() + Duration::from_millis(1800), id));
                }
            }
            pending.retain(|(t, id)| {
                if *t <= std::time::Instant::now() {
                    let _ = listener.RemoveNotification(*id);
                    false
                } else {
                    true
                }
            });
            seen = now;
            first = false;
            if fresh {
                let _ = PostMessageW(hw, WM_NOTE, WPARAM(0), LPARAM(0));
            }
        }
    });
}

/// Title = first line, body = the middle lines, and a last line after those (Discord's channel,
/// Telegram's account) goes next to the app name so it is always visible.
fn read(n: &windows::UI::Notifications::UserNotification) -> Option<Notice> {
    let info = n.AppInfo().ok()?;
    let app = info.DisplayInfo().ok()?.DisplayName().ok()?.to_string();
    let aumid = info.AppUserModelId().ok()?.to_string();
    let binding = n.Notification().ok()?.Visual().ok()?.GetBinding(&KnownNotificationBindings::ToastGeneric().ok()?).ok()?;
    let mut texts: Vec<String> = binding.GetTextElements().ok()?.into_iter().filter_map(|t| t.Text().ok()).map(|t| t.to_string()).filter(|t| !t.trim().is_empty()).collect();
    if texts.is_empty() {
        return None;
    }
    let first = texts[0].clone();
    // Telegram sends [chat, sender, message]; reorder to [sender, message, chat].
    if texts.len() >= 3 && app.to_lowercase().contains("telegram") {
        let chat = texts.remove(0);
        texts.push(chat);
    }
    let mut label = app;
    if texts.len() >= 3 {
        let extra = texts.pop().unwrap_or_default();
        label = format!("{label} \u{B7} {extra}");
    }
    let title = texts.remove(0);
    let exe = if aumid.is_empty() { String::new() } else { format!("shell:AppsFolder\\{aumid}") };
    let (image, silent) = avatar(&first, &aumid);
    // A toast marked silent leaves the sound to the app. Apps that do play their own (chat apps) are
    // left alone; any other silent one gets the Windows notification sound so it is not mute.
    if silent && !SELF_SOUND.iter().any(|a| aumid.to_lowercase().contains(a) || label.to_lowercase().contains(a)) {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
        unsafe {
            let _ = PlaySoundW(w!("Notification.Default"), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT);
        }
    }
    Some(Notice { app: label, title, text: texts.join(" "), exe, image, ..Default::default() })
}

/// The picture a toast carries (a Discord avatar, a Telegram chat photo). The listener API does not
/// expose it, so it is read out of Windows' notification store: the newest record with this title.
fn avatar(title: &str, aumid: &str) -> (String, bool) {
    let dir = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("Microsoft").join("Windows").join("Notifications");
    let needle = title.as_bytes();
    if needle.is_empty() {
        return (String::new(), false);
    }
    for name in ["wpndatabase.db-wal", "wpndatabase.db"] {
        // A runaway store must not stall the listener thread.
        if std::fs::metadata(dir.join(name)).map_or(true, |m| m.len() > 256 << 20) {
            continue;
        }
        let Some(buf) = read_tail(&dir.join(name), STORE_TAIL) else { continue };
        let Some(at) = buf.windows(needle.len()).rposition(|w| w == needle) else { continue };
        // Stay inside this toast's own XML, or a neighbour's picture (Discord's) gets picked up.
        let from = buf[..at].windows(6).rposition(|w| w == b"<toast").unwrap_or(at.saturating_sub(1500));
        let stop = (at + 2500).min(buf.len());
        let to = buf[at..stop].windows(8).position(|w| w == b"</toast>").map_or(stop, |p| at + p);
        let tail = &buf[from..to];
        let silent = tail.windows(13).any(|w| w == b"silent=\"true\"");
        // Find the <image ... placement="appLogoOverride" ...> tag (attributes in any order) and read its src.
        let marker = b"appLogoOverride";
        if let Some(k) = tail.windows(marker.len()).position(|w| w == marker) {
            let lo = tail[..k].iter().rposition(|&b| b == b'<').unwrap_or(0);
            let hi = tail[k..].iter().position(|&b| b == b'>').map_or(tail.len(), |p| k + p);
            let tag = String::from_utf8_lossy(&tail[lo..hi]).into_owned();
            if let Some(i) = tag.find("src=\"") {
                let rest = &tag[i + 5..];
                if let Some(end) = rest.find('"') {
                    let src = resolve_src(&rest[..end].replace("&amp;", "&"), aumid);
                    if let Some(path) = stash(&src) {
                        return (path, silent);
                    }
                }
            }
        }
        return (String::new(), silent);
    }
    (String::new(), false)
}

/// The newest toast records sit at the end of the store, so only that much is read per toast.
const STORE_TAIL: u64 = 16 << 20;
/// Most pictures kept in the avatar folder.
const MAX_AVATARS: usize = 200;

/// The last `max` bytes of a file (all of it when smaller).
fn read_tail(path: &std::path::Path, max: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::with_capacity(len.min(max) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Deletes the oldest files in `dir` until at most `keep` remain.
fn prune_dir(dir: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    let excess = files.len() - keep;
    for (_, p) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(p);
    }
}

/// Turns a toast picture reference into something `stash` can read: percent-decodes file URIs and
/// maps a packaged app's `ms-appdata:///local|roaming|temp/...` into its Packages folder.
/// http(s) and plain paths pass through.
fn resolve_src(src: &str, aumid: &str) -> String {
    let decode = |s: &str| {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
            match hex {
                Some(v) => {
                    out.push(v);
                    i += 3;
                }
                None => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    if let Some(r) = src.strip_prefix("file:///").or_else(|| src.strip_prefix("file://")) {
        return decode(r);
    }
    let lower = src.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("ms-appdata:///").map(|_| &src["ms-appdata:///".len()..]) {
        let (kind, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let pfn = aumid.split('!').next().unwrap_or("");
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let sub = match kind.to_ascii_lowercase().as_str() {
            "local" => "LocalState",
            "roaming" => "RoamingState",
            _ => "AC\\Temp",
        };
        if !pfn.is_empty() && !local.is_empty() {
            return format!("{local}\\Packages\\{pfn}\\{sub}\\{}", decode(tail).replace('/', "\\"));
        }
    }
    src.to_string()
}

/// Keeps a copy of a toast picture under %TEMP%\LumenAvatars: apps delete their own temp files
/// right after showing the toast, and http(s) pictures (Discord's CDN) are fetched with curl.exe.
fn stash(src: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let dir = std::env::temp_dir().join("LumenAvatars");
    std::fs::create_dir_all(&dir).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    let ext = if src.to_lowercase().contains(".webp") { "webp" } else if src.to_lowercase().contains(".jpg") || src.to_lowercase().contains(".jpeg") { "jpg" } else { "png" };
    let dest = dir.join(format!("{:016x}.{ext}", h.finish()));
    if !dest.exists() {
        static NEW_FILES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if NEW_FILES.fetch_add(1, Ordering::Relaxed) % 25 == 0 {
            prune_dir(&dir, MAX_AVATARS);
        }
        if src.starts_with("http://") || src.starts_with("https://") {
            use std::os::windows::process::CommandExt;
            let ok = std::process::Command::new("curl.exe")
                .args(["-sfL", "-m", "3", "-o"])
                .arg(&dest)
                .arg(src)
                .creation_flags(0x0800_0000)
                .status()
                .map_or(false, |s| s.success());
            if !ok {
                let _ = std::fs::remove_file(&dest);
                return None;
            }
        } else {
            let path = src.replace('/', "\\");
            std::fs::copy(&path, &dest).ok()?;
        }
    }
    Some(dest.to_string_lossy().into_owned())
}

/// Apps that play their own message sound when a toast is silent.
/// Matched as a substring, so "discord" also covers Canary, PTB, Vesktop, Legcord and the like.
const SELF_SOUND: [&str; 21] = [
    "discord", "vesktop", "legcord", "armcord", "telegram", "whatsapp", "slack", "teams", "signal", "spotify", "skype", "element", "viber", "zoom", "messenger", "beeper",
    "guilded", "revolt", "stoat", "wechat", "threema",
];

// ---- Muting Windows' own banners ----------------------------------------------------------

fn marker() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Lumen").join("banner_restore")
}

fn read_marker() -> Vec<(String, String)> {
    std::fs::read_to_string(marker()).unwrap_or_default().lines().filter_map(|l| l.split_once('\t')).map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

fn open(name: &str, access: REG_SAM_FLAGS) -> Option<HKEY> {
    let path = HSTRING::from(format!("{SETTINGS}\\{name}"));
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, 0, access, &mut key).is_ok().then_some(key) }
}

fn set_banner(key: HKEY, v: Option<u32>) {
    unsafe {
        match v {
            Some(v) => {
                let _ = RegSetValueExW(key, w!("ShowBanner"), 0, REG_DWORD, Some(&v.to_le_bytes()));
            }
            None => {
                let _ = RegDeleteValueW(key, w!("ShowBanner"));
            }
        }
    }
}

fn restore() {
    for (name, orig) in read_marker() {
        if let Some(key) = open(&name, KEY_SET_VALUE) {
            set_banner(key, orig.parse().ok());
            unsafe {
                let _ = RegCloseKey(key);
            }
        }
    }
    let _ = std::fs::remove_file(marker());
}

/// Turns every Windows toast off (all apps, Defender and system ones too) while Lumen shows them,
/// then puts the user's setting back. Windows still records the toasts, so they can be read.
pub fn set_blocked(block: bool) {
    BLOCK.store(block, Ordering::Relaxed);
    // Older builds forced ToastEnabled=0, which makes Discord/Telegram think notifications are off
    // and skip their own sound. Undo that if a record exists; the banner is hidden by the hider thread.
    let m = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Lumen").join("toast_restore");
    if let Ok(orig) = std::fs::read_to_string(&m) {
        let mut it = orig.split_whitespace();
        put_toast_enabled(it.next().and_then(|s| s.parse().ok()).unwrap_or(1));
        put_noc(NOC_TOASTS, it.next().and_then(|s| s.parse().ok()));
        put_noc(NOC_SOUND, it.next().and_then(|s| s.parse().ok()));
        let _ = std::fs::remove_file(&m);
    }
    // Windows' native sound stays on; undo any muting an older build left behind.
    mute_sounds(false);
}

// Windows plays toast sounds through these sound-scheme events; blank them while blocking.
const SOUND_EVENTS: [&str; 5] = ["Notification.Default", "Notification.IM", "Notification.Mail", "Notification.Reminder", "Notification.SMS"];

fn sound_path(ev: &str) -> HSTRING {
    HSTRING::from(format!(r"AppEvents\Schemes\Apps\.Default\{ev}\.Current"))
}

fn sound_get(ev: &str) -> Option<(u32, String)> {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &sound_path(ev), 0, KEY_QUERY_VALUE, &mut key).is_err() {
            return None;
        }
        let mut buf = vec![0u8; 1024];
        let (mut ty, mut len) = (REG_VALUE_TYPE(0), 1024u32);
        let ok = RegQueryValueExW(key, PCWSTR::null(), None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut len)).is_ok();
        let _ = RegCloseKey(key);
        if !ok {
            return None;
        }
        let wide: Vec<u16> = buf[..len as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Some((ty.0, String::from_utf16_lossy(&wide)))
    }
}

fn sound_put(ev: &str, ty: u32, val: &str) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &sound_path(ev), 0, KEY_SET_VALUE, &mut key).is_ok() {
            let bytes: Vec<u8> = val.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
            let _ = RegSetValueExW(key, PCWSTR::null(), 0, REG_VALUE_TYPE(ty), Some(&bytes));
            let _ = RegCloseKey(key);
        }
    }
}

fn mute_sounds(block: bool) {
    let f = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Lumen").join("sound_restore");
    if block {
        if !f.exists() {
            let lines: Vec<String> = SOUND_EVENTS.iter().filter_map(|e| sound_get(e).map(|(t, v)| format!("{e}\t{t}\t{v}"))).collect();
            let _ = std::fs::write(&f, lines.join("\n"));
        }
        for e in SOUND_EVENTS {
            sound_put(e, 1, "");
        }
    } else if let Ok(s) = std::fs::read_to_string(&f) {
        for l in s.lines() {
            let mut p = l.splitn(3, '\t');
            if let (Some(e), Some(t), Some(v)) = (p.next(), p.next().and_then(|t| t.parse().ok()), p.next()) {
                sound_put(e, t, v);
            }
        }
        let _ = std::fs::remove_file(&f);
    }
}

const NOC_TOASTS: &str = "NOC_GLOBAL_SETTING_TOASTS_ENABLED";
const NOC_SOUND: &str = "NOC_GLOBAL_SETTING_ALLOW_NOTIFICATION_SOUND";

fn noc_get(name: &str) -> Option<u32> {
    let (mut v, mut size) = (0u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Notifications\Settings"), &HSTRING::from(name), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size)).is_ok().then_some(v) }
}

fn put_noc(name: &str, v: Option<u32>) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Notifications\Settings"), 0, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).is_ok() {
            let n = HSTRING::from(name);
            match v {
                Some(v) => {
                    let _ = RegSetValueExW(key, &n, 0, REG_DWORD, Some(&v.to_le_bytes()));
                }
                None => {
                    let _ = RegDeleteValueW(key, &n);
                }
            }
            let _ = RegCloseKey(key);
        }
    }
}

fn toast_enabled() -> Option<u32> {
    let (mut v, mut size) = (0u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\PushNotifications"), w!("ToastEnabled"), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size)).is_ok().then_some(v) }
}

fn put_toast_enabled(v: u32) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\PushNotifications"), 0, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).is_ok() {
            let _ = RegSetValueExW(key, w!("ToastEnabled"), 0, REG_DWORD, Some(&v.to_le_bytes()));
            let _ = RegCloseKey(key);
        }
    }
}

/// An older build muted each app's banner separately; undo that if it left a record.
fn legacy_restore() {
    restore();
}

/// Fires a REAL Windows toast as a known app (0 Discord, 1 Telegram, 2 generic app) so the whole
/// path is exercised: banner hiding, capture, app sound rule and the Lumen card.
/// Sent through a hidden PowerShell (avoids the heavy `Data_Xml_Dom` windows feature, which crashes rustc).
pub fn test_toast(kind: u8) {
    use std::os::windows::process::CommandExt;
    let (aumid, lines): (&str, [&str; 3]) = match kind {
        0 => ("com.squirrel.Discord.Discord", ["bob", "build is green", "#dev-chat, Lumen HQ"]),
        1 => ("Telegram.TelegramDesktop", ["Family", "Mom", "dinner at 7?"]),
        _ => (
            r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe",
            ["Windows app", "A toast from an app with no sound of its own", ""],
        ),
    };
    let mut xml = String::from("<toast><visual><binding template='ToastGeneric'>");
    for l in lines.iter().filter(|l| !l.is_empty()) {
        xml.push_str(&format!("<text>{}</text>", l.replace('&', "&amp;").replace('<', "&lt;")));
    }
    xml.push_str("</binding></visual>");
    // Discord and Telegram play their own sound and mark their toasts silent; mimic that so the
    // test shows what really happens: silent for them, the Windows sound for any other app.
    if kind < 2 {
        xml.push_str("<audio silent='true'/>");
    }
    xml.push_str("</toast>");
    let script = format!(
        "[void][Windows.UI.Notifications.ToastNotificationManager,Windows.UI.Notifications,ContentType=WindowsRuntime];\
         [void][Windows.Data.Xml.Dom.XmlDocument,Windows.Data.Xml.Dom.XmlDocument,ContentType=WindowsRuntime];\
         $x=New-Object Windows.Data.Xml.Dom.XmlDocument;$x.LoadXml(\"{}\");\
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{}').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
        xml.replace('"', "`\""),
        aumid
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(0x0800_0000)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_picture_refs() {
        assert_eq!(resolve_src("file:///C:/a%20b/c%28d%29.png", ""), "C:/a b/c(d).png");
        assert_eq!(resolve_src("https://cdn.discordapp.com/a.png?x=1", "x"), "https://cdn.discordapp.com/a.png?x=1");
        std::env::set_var("LOCALAPPDATA", r"C:\L");
        assert_eq!(resolve_src("ms-appdata:///local/img/a%20b.png", "Pkg_123!App"), r"C:\L\Packages\Pkg_123\LocalState\img\a b.png");
    }

    #[test]
    fn hider_is_fast_only_just_after_a_toast() {
        assert_eq!(hider_sleep(Duration::ZERO, true), Duration::from_millis(6));
        assert_eq!(hider_sleep(HIDER_BURST, true), Duration::from_millis(40));
        assert_eq!(hider_sleep(HIDER_BURST, false), Duration::from_millis(6));
    }

    #[test]
    fn read_tail_returns_only_the_end() {
        let p = std::env::temp_dir().join("lumen_tail_test.bin");
        std::fs::write(&p, b"0123456789").unwrap();
        assert_eq!(read_tail(&p, 4).unwrap(), b"6789");
        assert_eq!(read_tail(&p, 100).unwrap(), b"0123456789");
        let _ = std::fs::remove_file(&p);
        assert!(read_tail(&p, 4).is_none());
    }

    #[test]
    fn prune_dir_keeps_newest() {
        let d = std::env::temp_dir().join("lumen_prune_test");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..5 {
            std::fs::write(d.join(format!("{i}.png")), b"x").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        prune_dir(&d, 2);
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["3.png", "4.png"]);
        prune_dir(&d, 10);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
