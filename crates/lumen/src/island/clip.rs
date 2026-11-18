//! Clipboard history: reads text and file lists, stores them on disk, copies them back.
//! Files are only ever stored as paths and put back on the clipboard; nothing is opened or run.

use serde::{Deserialize, Serialize};
use windows::core::{w, HRESULT};
use windows::Win32::Foundation::{GlobalFree, BOOL, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_NOTIMPL, E_OUTOFMEMORY, OLE_E_ADVISENOTSUPPORTED, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HANDLE, HGLOBAL, HWND, S_OK};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Com::{IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{DoDragDrop, IDropSource, IDropSource_Impl, CF_DIB, CF_HDROP, CF_TEXT, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{DragQueryFileW, SHCreateStdEnumFmtEtc, DROPFILES, HDROP};

const MAX_TEXT: usize = 20_000;
const MAX_ENTRIES: usize = 200;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Entry {
    /// Unix seconds.
    pub t: u64,
    /// `s` is newline-separated file paths.
    pub files: bool,
    pub s: String,
    /// Total bytes of the files, measured once when copied and saved with the history.
    #[serde(default)]
    pub size: u64,
    /// Pinned entries never expire, never fall off the end, and survive Clear.
    #[serde(default)]
    pub pin: bool,
    /// A copied image: `files` is true and `s` is the PNG saved next to the history.
    #[serde(default)]
    pub img: bool,
}

/// Most unpinned images kept on disk.
const MAX_IMGS: usize = 20;
/// Images larger than this are not saved.
const MAX_IMG_BYTES: usize = 16 << 20;

/// Deletes the saved picture of an image entry (only inside our own image folder).
fn drop_img(e: &Entry) {
    if e.img && std::path::Path::new(&e.s).starts_with(img_dir()) {
        let _ = std::fs::remove_file(&e.s);
    }
}

fn img_dir() -> std::path::PathBuf {
    let mut p = crate::config::path();
    p.set_file_name("island_img");
    p
}

/// Saves a copied image (clipboard "PNG", else a DIB written out as .bmp) and returns its path.
/// Skips private content and anything over 16 MB.
pub fn read_image(owner: HWND) -> Option<String> {
    unsafe {
        if IsClipboardFormatAvailable(RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"))).is_ok() {
            return None;
        }
        let png = RegisterClipboardFormatW(w!("PNG"));
        let has_png = IsClipboardFormatAvailable(png).is_ok();
        if !has_png && IsClipboardFormatAvailable(CF_DIB.0 as u32).is_err() {
            return None;
        }
        if !open(owner) {
            return None;
        }
        let bytes = (|| {
            let h = GetClipboardData(if has_png { png } else { CF_DIB.0 as u32 }).ok()?;
            let g = HGLOBAL(h.0);
            let n = GlobalSize(g);
            let p = GlobalLock(g) as *const u8;
            if p.is_null() {
                return None;
            }
            let d = std::slice::from_raw_parts(p, n.min(MAX_IMG_BYTES + 1)).to_vec();
            let _ = GlobalUnlock(g);
            (d.len() <= MAX_IMG_BYTES && d.len() > 40).then_some(d)
        })();
        let _ = CloseClipboard();
        let mut d = bytes?;
        let ext = if has_png {
            "png"
        } else {
            // Prefix a BMP file header: pixels start after the info header, colour table and bit masks.
            let u32_at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as usize;
            let (hs, bits, comp, used) = (u32_at(0), u16::from_le_bytes([d[14], d[15]]) as usize, u32_at(16), u32_at(32));
            let colors = if bits <= 8 { if used > 0 { used } else { 1 << bits } } else { used };
            let masks = if comp == 3 && hs == 40 { 12 } else { 0 };
            let off = 14 + hs + colors * 4 + masks;
            let mut f = Vec::with_capacity(d.len() + 14);
            f.extend_from_slice(b"BM");
            f.extend_from_slice(&((d.len() + 14) as u32).to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&(off as u32).to_le_bytes());
            f.extend_from_slice(&d);
            d = f;
            "bmp"
        };
        let dir = img_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let p = dir.join(format!("{}.{ext}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()));
        std::fs::write(&p, d).ok()?;
        Some(p.to_string_lossy().into_owned())
    }
}

fn files_size(s: &str) -> u64 {
    s.lines().filter_map(|p| std::fs::metadata(p).ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
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
        // Entries saved before sizes existed: measure once, then they persist.
        for e in s.items.iter_mut().filter(|e| e.files && e.size == 0) {
            e.size = files_size(&e.s);
            s.dirty = true;
        }
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
        let (mut keep, mut imgs) = (0, 0);
        self.items.retain(|e| {
            keep += 1;
            let ok = e.pin
                || (e.t >= cutoff
                    && keep <= MAX_ENTRIES
                    && (!e.img || {
                        imgs += 1;
                        imgs <= MAX_IMGS
                    }));
            if !ok {
                drop_img(e);
            }
            ok
        });
        if self.items.len() != n {
            self.dirty = true;
        }
    }

    /// Adds a copied image already saved at `path`.
    pub fn add_image(&mut self, path: String) {
        self.items.insert(0, Entry { t: now(), files: true, size: files_size(&path), s: path, pin: false, img: true });
        self.prune();
        self.dirty = true;
    }

    pub fn add(&mut self, files: bool, s: String) {
        if s.trim().is_empty() {
            return;
        }
        // Move an existing identical entry to the top instead of duplicating it.
        let pin = self.items.iter().any(|e| e.pin && e.files == files && e.s == s);
        self.items.retain(|e| !(e.files == files && e.s == s));
        self.items.insert(0, Entry { t: now(), files, size: if files { files_size(&s) } else { 0 }, s, pin, img: false });
        self.prune();
        self.dirty = true;
    }

    pub fn toggle_pin(&mut self, i: usize) {
        if let Some(e) = self.items.get_mut(i) {
            e.pin = !e.pin;
            self.dirty = true;
        }
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            drop_img(&self.items.remove(i));
            self.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        self.items.retain(|e| {
            if !e.pin {
                drop_img(e);
            }
            e.pin
        });
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
        let data: Option<IDataObject> = if e.files { files_data(e) } else { text_data(e) };
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

/// Plain text data object: unicode (and ANSI when ASCII) served straight from memory. Chat apps
/// (Telegram, Discord) are pickier about the shell's generic object than about this one.
#[windows::core::implement(IDataObject)]
struct TextData {
    fmts: Vec<(u16, Vec<u8>)>,
}

impl TextData {
    fn find(&self, f: *const FORMATETC) -> Option<&Vec<u8>> {
        let f = unsafe { &*f };
        if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return None;
        }
        self.fmts.iter().find(|(c, _)| *c == f.cfFormat).map(|(_, d)| d)
    }
}

impl IDataObject_Impl for TextData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let data = self.find(pformatetcin).ok_or(windows::core::Error::from(DV_E_FORMATETC))?;
        unsafe {
            let mem = GlobalAlloc(GMEM_MOVEABLE, data.len())?;
            let p = GlobalLock(mem) as *mut u8;
            if p.is_null() {
                let _ = GlobalFree(mem);
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let _ = GlobalUnlock(mem);
            Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: mem }, pUnkForRelease: std::mem::ManuallyDrop::new(None) })
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if self.find(pformatetc).is_some() { S_OK } else { DV_E_FORMATETC }
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        DATA_S_SAMEFORMATETC
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let list: Vec<FORMATETC> = self
            .fmts
            .iter()
            .map(|(c, _)| FORMATETC { cfFormat: *c, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 })
            .collect();
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Option<&IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// CF_HDROP bytes: DROPFILES header (pFiles=20, pt=(0,0), fNC=0, fWide=1), then NUL-separated
/// wide paths ended by a double NUL.
fn hdrop(paths: &[&str]) -> Vec<u8> {
    let mut b: Vec<u8> = [20u32, 0, 0, 0, 1].iter().flat_map(|v| v.to_le_bytes()).collect();
    for p in paths {
        b.extend(p.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()));
    }
    b.extend([0, 0]);
    b
}

fn text_data(e: &Entry) -> Option<IDataObject> {
    let mut fmts = vec![(CF_UNICODETEXT.0, e.s.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>())];
    // Many drop targets (older editors, terminals) only ask for ANSI text; offer it for ASCII.
    if e.s.is_ascii() {
        fmts.push((CF_TEXT.0, e.s.bytes().chain(std::iter::once(0)).collect()));
    }
    // Chat apps often only accept files: also offer the text as a temp .txt (CF_HDROP).
    let tmp = std::env::temp_dir().join("Lumen-clip.txt");
    if std::fs::write(&tmp, &e.s).is_ok() {
        fmts.push((CF_HDROP.0, hdrop(&[&tmp.to_string_lossy()])));
    }
    Some(TextData { fmts }.into())
}

fn files_data(e: &Entry) -> Option<IDataObject> {
    let paths: Vec<&str> = e.s.lines().filter(|p| std::path::Path::new(p).exists()).collect();
    if paths.is_empty() {
        return None;
    }
    Some(TextData { fmts: vec![(CF_HDROP.0, hdrop(&paths))] }.into())
}
