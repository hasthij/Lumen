//! Clipboard history: reads text and file lists, stores them on disk, copies them back.
//! Files are only ever stored as paths and put back on the clipboard; nothing is opened or run.

use serde::{Deserialize, Serialize};
use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HANDLE, HGLOBAL, HWND, S_OK};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{DoDragDrop, IDropSource, IDropSource_Impl, CF_HDROP, CF_TEXT, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{DragQueryFileW, SHCreateDataObject, SHParseDisplayName, DROPFILES, HDROP};

const MAX_TEXT: usize = 20_000;
const MAX_ENTRIES: usize = 200;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Entry {
    /// Unix seconds.
    pub t: u64,
    /// `s` is newline-separated file paths.
    pub files: bool,
    pub s: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Disk {
    e: Vec<Entry>,
}

/// Newest first.
pub struct Store {
    pub items: Vec<Entry>,
    days: u32,
    dirty: bool,
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn path() -> std::path::PathBuf {
    let mut p = crate::config::path();
    p.set_file_name("island_history.toml");
    p
}

impl Store {
    pub fn load(days: u32) -> Store {
        let items = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| toml::from_str::<Disk>(&s).ok())
            .map(|d| d.e)
            .unwrap_or_default();
        let mut s = Store { items, days, dirty: false };
        s.prune();
        s
    }

    pub fn set_days(&mut self, days: u32) {
        if days != self.days {
            self.days = days;
            self.prune();
        }
    }

    fn prune(&mut self) {
        let cutoff = now().saturating_sub(self.days as u64 * 86_400);
        let n = self.items.len();
        self.items.retain(|e| e.t >= cutoff);
        self.items.truncate(MAX_ENTRIES);
        if self.items.len() != n {
            self.dirty = true;
        }
    }

    pub fn add(&mut self, files: bool, s: String) {
        if s.trim().is_empty() {
            return;
        }
        // Move an existing identical entry to the top instead of duplicating it.
        self.items.retain(|e| !(e.files == files && e.s == s));
        self.items.insert(0, Entry { t: now(), files, s });
        self.prune();
        self.dirty = true;
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
            self.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.dirty = true;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Ok(s) = toml::to_string(&Disk { e: self.items.clone() }) {
            let p = path();
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, s).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
                self.dirty = false;
            }
        }
    }
}

fn open(owner: HWND) -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

pub fn has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Current clipboard as (is_files, text). None when empty, unreadable or marked private
/// (password managers set the exclude format).
pub fn read(owner: HWND) -> Option<(bool, String)> {
    unsafe {
        if IsClipboardFormatAvailable(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"))).is_ok() {
            return None;
        }
        let files = IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok();
        let text = IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok();
        if !files && !text {
            return None;
        }
        if !open(owner) {
            return None;
        }
        let mut out = if files {
            (|| {
                let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
                let drop = HDROP(h.0);
                let n = DragQueryFileW(drop, u32::MAX, None);
                if n > 50 {
                    crate::info!("island: clipboard has {n} files, skipped");
                    return None; // too many to store whole; skip rather than keep a partial list
                }
                let mut paths = Vec::new();
                for i in 0..n {
                    let len = DragQueryFileW(drop, i, None) as usize;
                    let mut buf = vec![0u16; len + 1];
                    let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                    paths.push(String::from_utf16_lossy(&buf[..got.min(len)]));
                }
                Some((true, paths.join("\n")))
            })()
        } else {
            (|| {
                let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u16;
                if p.is_null() {
                    return None;
                }
                let mut len = 0;
                while len <= MAX_TEXT && *p.add(len) != 0 {
                    len += 1;
                }
                let s = (len <= MAX_TEXT).then(|| String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                let _ = GlobalUnlock(g);
                s.map(|s| (false, s)) // oversized text is skipped, not stored truncated
            })()
        };
        if out.is_none() && files && text {
            out = (|| {
                let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u16;
                if p.is_null() {
                    return None;
                }
                let mut len = 0;
                while len <= MAX_TEXT && *p.add(len) != 0 {
                    len += 1;
                }
                let s = (len <= MAX_TEXT).then(|| String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
                let _ = GlobalUnlock(g);
                s.map(|s| (false, s))
            })();
        }
        let _ = CloseClipboard();
        out
    }
}

/// Builds the global memory for an entry: (clipboard format, handle). Files only if they still exist.
fn build(e: &Entry) -> Option<(u32, HGLOBAL)> {
    unsafe {
        if e.files {
            let list: Vec<&str> = e.s.lines().filter(|p| std::path::Path::new(p).exists()).collect();
            if list.is_empty() {
                return None;
            }
            let mut wide: Vec<u16> = Vec::new();
            for p in &list {
                wide.extend(p.encode_utf16());
                wide.push(0);
            }
            wide.push(0);
            let head = std::mem::size_of::<DROPFILES>();
            let mem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, head + wide.len() * 2).ok()?;
            let ptr = GlobalLock(mem) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(mem);
                return None;
            }
            let df = ptr as *mut DROPFILES;
            (*df).pFiles = head as u32;
            (*df).fWide = true.into();
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr.add(head), wide.len() * 2);
            let _ = GlobalUnlock(mem);
            Some((CF_HDROP.0 as u32, mem))
        } else {
            let wide: Vec<u16> = e.s.encode_utf16().chain(std::iter::once(0)).collect();
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(mem);
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(mem);
            Some((CF_UNICODETEXT.0 as u32, mem))
        }
    }
}

/// Puts an entry back on the clipboard. Files go back as a plain file list (a copy), and only
/// if they still exist.
pub fn write(owner: HWND, e: &Entry) -> bool {
    let Some((format, mem)) = build(e) else { return false };
    unsafe {
        if !open(owner) {
            let _ = GlobalFree(mem);
            return false;
        }
        let _ = EmptyClipboard();
        let ok = SetClipboardData(format, HANDLE(mem.0)).is_ok();
        let _ = CloseClipboard();
        if !ok {
            let _ = GlobalFree(mem);
        }
        ok
    }
}

#[windows::core::implement(IDropSource)]
struct Src;

impl IDropSource_Impl for Src_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drags an entry out of the panel (blocks until dropped or cancelled). Text drops as text; files go
/// through a real shell data object (built from item ids) so Explorer, browsers and chat apps accept them.
pub fn drag(e: &Entry) {
    unsafe {
        let data: Option<IDataObject> = if e.files { shell_data(e) } else { text_data(e) };
        let Some(data) = data else {
            crate::warn!("island: drag skipped, nothing to drag");
            return;
        };
        let src: IDropSource = Src.into();
        let mut effect = DROPEFFECT_NONE;
        let r = DoDragDrop(&data, &src, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK, &mut effect);
        crate::info!("island: drag result {:?} effect {}", r, effect.0);
    }
}

unsafe fn text_data(e: &Entry) -> Option<IDataObject> {
    let (format, mem) = build(e)?;
    let data = SHCreateDataObject::<_, IDataObject>(None, None, None::<&IDataObject>).ok();
    let Some(data) = data else {
        let _ = GlobalFree(mem);
        return None;
    };
    let fmt = FORMATETC { cfFormat: format as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: mem }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
    if data.SetData(&fmt, &medium, true).is_err() {
        let _ = GlobalFree(mem);
        return None;
    }
    // Many drop targets (older editors, terminals) only ask for ANSI text; offer it for ASCII.
    if e.s.is_ascii() {
        let bytes: Vec<u8> = e.s.bytes().chain(std::iter::once(0)).collect();
        if let Ok(m) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
            let p = GlobalLock(m) as *mut u8;
            if p.is_null() {
                let _ = GlobalFree(m);
            } else {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                let _ = GlobalUnlock(m);
                let f = FORMATETC { cfFormat: CF_TEXT.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
                let med = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: m }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
                if data.SetData(&f, &med, true).is_err() {
                    let _ = GlobalFree(m);
                }
            }
        }
    }
    Some(data)
}

unsafe fn shell_data(e: &Entry) -> Option<IDataObject> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    for p in e.s.lines().filter(|p| std::path::Path::new(p).exists()) {
        let wide: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_ok() && !pidl.is_null() {
            pidls.push(pidl);
        }
    }
    if pidls.is_empty() {
        return None;
    }
    let list: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
    let data = SHCreateDataObject::<_, IDataObject>(None, Some(&list), None::<&IDataObject>);
    for p in pidls {
        CoTaskMemFree(Some(p as *const _));
    }
    match data {
        Ok(d) => Some(d),
        Err(err) => {
            crate::warn!("island: SHCreateDataObject failed: {err}");
            None
        }
    }
}
