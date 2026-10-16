//! Lumen's taskbar helper. Windows loads this DLL into Explorer when Lumen calls
//! `InitializeXamlDiagnosticsEx` (the API Visual Studio's Live Visual Tree uses).
//!
//! It watches Explorer's XAML tree for the taskbar's background rectangles
//! (`Taskbar.TaskbarFrame` › … › `Rectangle#BackgroundFill` / `#BackgroundStroke`) and
//! sets their fill. Lumen controls it through a message-only window:
//! `WM_APP+1` with wParam = ARGB colour applies it; `WM_APP+2` restores Windows' own look.
//!
//! Getting in: Lumen sets a temporary window hook on the taskbar's thread, which makes
//! Windows load this DLL into Explorer. `DllMain` then starts a thread that calls
//! `InitializeXamlDiagnosticsEx` from inside Explorer (the same approach TranslucentTB
//! uses), which pins the DLL and creates the TAP object below. Lumen then removes the hook.
//!
//! Everything here runs inside Explorer, so every entry point catches panics and fails
//! quietly instead of crashing the shell.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};

use windows::core::{implement, interface, w, Interface, BSTR, GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR};
use windows::Win32::Foundation::{BOOL, CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_POINTER, HWND, LPARAM, LRESULT, S_FALSE, S_OK, WPARAM};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Must match `TAP_CLSID` in Lumen.
pub const CLSID_TAP: GUID = GUID::from_u128(0x5a8d2e3c_7b41_4f0e_9c6a_1d2b3e4f5a6b);

const WM_LUMEN_APPLY: u32 = WM_APP + 1;
const WM_LUMEN_RESTORE: u32 = WM_APP + 2;

// ---- XAML diagnostics interfaces (from xamlOM.h; not in the windows crate) -------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SourceInfo {
    file_name: *const u16,
    line: u32,
    column: u32,
    char_position: u32,
    hash: *const u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ParentChildRelation {
    parent: u64,
    child: u64,
    child_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VisualElement {
    handle: u64,
    src: SourceInfo,
    type_name: *const u16,
    name: *const u16,
    num_children: u32,
}

#[interface("AA7A8931-80E4-4FEC-8F3B-553F87B4966E")]
unsafe trait IVisualTreeServiceCallback: IUnknown {
    fn OnVisualTreeChange(&self, relation: ParentChildRelation, element: VisualElement, mutation: i32) -> HRESULT;
}

#[interface("BAD9EB88-AE77-4397-B948-5FA2DB0A19EA")]
unsafe trait IVisualTreeServiceCallback2: IVisualTreeServiceCallback {
    fn OnElementStateChanged(&self, element: u64, state: i32, context: *const u16) -> HRESULT;
}

#[interface("A593B11A-D17F-48BB-8F66-83910731C8A5")]
unsafe trait IVisualTreeService: IUnknown {
    fn AdviseVisualTreeChange(&self, callback: *mut c_void) -> HRESULT;
    fn UnadviseVisualTreeChange(&self, callback: *mut c_void) -> HRESULT;
    fn GetEnums(&self, count: *mut u32, enums: *mut *mut c_void) -> HRESULT;
    fn CreateInstance(&self, type_name: *const u16, value: *const u16, handle: *mut u64) -> HRESULT;
    fn GetPropertyValuesChain(&self, handle: u64, sc: *mut u32, s: *mut *mut c_void, pc: *mut u32, p: *mut *mut c_void) -> HRESULT;
    fn SetProperty(&self, handle: u64, value: u64, index: u32) -> HRESULT;
    fn ClearProperty(&self, handle: u64, index: u32) -> HRESULT;
}

#[interface("130F5136-EC43-4F61-89C7-9801A36D2E95")]
unsafe trait IVisualTreeService2: IVisualTreeService {
    fn GetPropertyIndex(&self, object: u64, name: *const u16, index: *mut u32) -> HRESULT;
}

#[interface("18C9E2B6-3F43-4116-9F2B-FF935D7770D2")]
unsafe trait IXamlDiagnostics: IUnknown {
    fn GetDispatcher(&self, out: *mut *mut c_void) -> HRESULT;
    fn GetUiLayer(&self, out: *mut *mut c_void) -> HRESULT;
    fn GetApplication(&self, out: *mut *mut c_void) -> HRESULT;
    fn GetIInspectableFromHandle(&self, handle: u64, out: *mut *mut c_void) -> HRESULT;
    fn GetHandleFromIInspectable(&self, obj: *mut c_void, handle: *mut u64) -> HRESULT;
    fn HitTest(&self, rect: windows::Win32::Foundation::RECT, count: *mut u32, handles: *mut *mut u64) -> HRESULT;
    fn RegisterInstance(&self, obj: *mut c_void, handle: *mut u64) -> HRESULT;
    fn GetInitializationData(&self, data: *mut *const u16) -> HRESULT;
}

// ---- Windows.UI.Xaml interfaces (the windows crate has no XAML namespaces) --------------

/// ABI::Windows::UI::Color: four bytes, alpha first.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UiColor {
    a: u8,
    r: u8,
    g: u8,
    b: u8,
}

/// Windows.UI.Xaml.Shapes.IShape (only Fill is used; the vtable order matches the SDK header).
#[interface("786f2b75-9aa0-454d-ae06-a2466e37c832")]
unsafe trait IShape: IUnknown {
    fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
    fn GetRuntimeClassName(&self, name: *mut *mut c_void) -> HRESULT;
    fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
    fn get_Fill(&self, value: *mut *mut c_void) -> HRESULT;
    fn put_Fill(&self, value: *mut c_void) -> HRESULT;
}

/// Windows.UI.Xaml.Media.IBrush (identity only).
#[interface("8806a321-1e06-422c-a1cc-01696559e021")]
unsafe trait IBrush: IUnknown {
    fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
    fn GetRuntimeClassName(&self, name: *mut *mut c_void) -> HRESULT;
    fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
}

/// Windows.UI.Xaml.Media.ISolidColorBrushFactory.
#[interface("d935ce0c-86f5-4da6-8a27-b1619ef7f92b")]
unsafe trait ISolidColorBrushFactory: IUnknown {
    fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
    fn GetRuntimeClassName(&self, name: *mut *mut c_void) -> HRESULT;
    fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
    fn CreateInstanceWithColor(&self, color: UiColor, value: *mut *mut c_void) -> HRESULT;
}

// ---- State ------------------------------------------------------------------------

/// The XAML diagnostics service. Its methods may be called from any thread.
struct Service(IVisualTreeService2);
unsafe impl Send for Service {}
unsafe impl Sync for Service {}

/// The diagnostics object, used to turn a tree handle into the real XAML element.
struct Diag(IXamlDiagnostics);
unsafe impl Send for Diag {}
unsafe impl Sync for Diag {}

/// Brushes saved before Lumen changed them, so "restore" puts Windows' own look back.
struct Originals(HashMap<u64, Option<IBrush>>);
unsafe impl Send for Originals {}

static DIAG: OnceLock<Diag> = OnceLock::new();
static ORIGINALS: LazyLock<Mutex<Originals>> = LazyLock::new(|| Mutex::new(Originals(HashMap::new())));
static SERVICE: OnceLock<Service> = OnceLock::new();
/// Desired look: bit 32 set = Lumen's colour active, low 32 bits = ARGB.
static DESIRED: AtomicU64 = AtomicU64::new(0);
static WINDOW: AtomicIsize = AtomicIsize::new(0);

#[derive(Default)]
struct Tree {
    parents: HashMap<u64, u64>,
    types: HashMap<u64, String>,
    /// Background rectangles inside a taskbar frame: handle -> is the stroke (top line).
    targets: HashMap<u64, bool>,
}

// XAML calls back on whichever thread Explorer uses and Lumen's commands arrive on the
// control thread, so the tree lives in a global, never in thread-local storage.
static TREE: LazyLock<Mutex<Tree>> = LazyLock::new(|| Mutex::new(Tree::default()));

/// Never hold this guard across a call into the XAML service: those calls can re-enter
/// `OnVisualTreeChange` on the same thread.
fn tree() -> MutexGuard<'static, Tree> {
    TREE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Appends a line to the taskbar-helper log in Lumen's folder (this runs inside Explorer).
fn log(msg: &str) {
    use std::io::Write;
    let dev = std::path::PathBuf::from(r"C:\Main\Lumen\lumen-logs");
    let dir = if dev.is_dir() {
        dev
    } else {
        let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
        std::path::Path::new(&base).join("Lumen")
    };
    let path = dir.join("taskbar-helper.log");
    if std::fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

fn wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let mut n = 0;
        while *p.add(n) != 0 && n < 4096 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

fn under_taskbar(tree: &Tree, mut h: u64) -> bool {
    for _ in 0..64 {
        match tree.parents.get(&h) {
            Some(&p) => {
                if tree.types.get(&p).is_some_and(|t| t == "Taskbar.TaskbarFrame") {
                    return true;
                }
                h = p;
            }
            None => return false,
        }
    }
    false
}

/// A new SolidColorBrush of `color`, as an IBrush.
unsafe fn solid_brush(color: UiColor) -> Option<IBrush> {
    use windows::core::HSTRING;
    use windows::Win32::System::WinRT::RoGetActivationFactory;
    let factory: ISolidColorBrushFactory = RoGetActivationFactory(&HSTRING::from("Windows.UI.Xaml.Media.SolidColorBrush")).ok()?;
    let mut raw: *mut c_void = std::ptr::null_mut();
    factory.CreateInstanceWithColor(color, &mut raw).ok().ok()?;
    if raw.is_null() {
        return None;
    }
    // The factory returns ISolidColorBrush; ask it for IBrush.
    windows::core::IUnknown::from_raw(raw).cast::<IBrush>().ok()
}

/// Sets (or restores) the fill of one background rectangle by changing the real XAML
/// object, the way TranslucentTB does. Must run on the XAML UI thread.
fn style(handle: u64, stroke: bool) -> bool {
    let Some(diag) = DIAG.get() else { return false };
    let desired = DESIRED.load(Ordering::SeqCst);
    unsafe {
        let mut raw: *mut c_void = std::ptr::null_mut();
        if diag.0.GetIInspectableFromHandle(handle, &mut raw).is_err() || raw.is_null() {
            return false;
        }
        let inspectable = windows::core::IInspectable::from_raw(raw);
        let Ok(shape) = inspectable.cast::<IShape>() else {
            log("element is not a shape");
            return false;
        };
        let mut originals = ORIGINALS.lock().unwrap_or_else(|e| e.into_inner());
        if desired >> 32 == 0 {
            // Restore what Windows had.
            if let Some(orig) = originals.0.remove(&handle) {
                let ptr = orig.as_ref().map_or(std::ptr::null_mut(), |b| b.as_raw());
                let _ = shape.put_Fill(ptr);
            }
            return true;
        }
        if !originals.0.contains_key(&handle) {
            let mut cur: *mut c_void = std::ptr::null_mut();
            let saved = if shape.get_Fill(&mut cur).is_ok() && !cur.is_null() {
                windows::core::IUnknown::from_raw(cur).cast::<IBrush>().ok()
            } else {
                None
            };
            originals.0.insert(handle, saved);
        }
        drop(originals);
        // The top border line is always hidden while Lumen styles the taskbar.
        let argb = if stroke { 0 } else { desired as u32 };
        let color = UiColor { a: (argb >> 24) as u8, r: (argb >> 16) as u8, g: (argb >> 8) as u8, b: argb as u8 };
        let Some(brush) = solid_brush(color) else {
            log("could not create a brush");
            return false;
        };
        let hr = shape.put_Fill(brush.as_raw());
        log(&format!("fill #{argb:08X} on {handle}: {hr:?}"));
        hr.is_ok()
    }
}

fn restyle_all() {
    let targets: Vec<(u64, bool)> = tree().targets.iter().map(|(&h, &st)| (h, st)).collect();
    for (h, stroke) in targets {
        style(h, stroke);
    }
}

// ---- Control window ------------------------------------------------------------------

unsafe extern "system" fn control_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let r = catch_unwind(AssertUnwindSafe(|| match msg {
        WM_LUMEN_APPLY => {
            DESIRED.store((1u64 << 32) | (wp.0 as u64 & 0xFFFF_FFFF), Ordering::SeqCst);
            restyle_all();
            Some(LRESULT(1))
        }
        WM_LUMEN_RESTORE => {
            DESIRED.store(0, Ordering::SeqCst);
            restyle_all();
            Some(LRESULT(1))
        }
        _ => None,
    }));
    match r {
        Ok(Some(l)) => l,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Creates the control window on the *current* thread. It must be the XAML UI thread (the one
/// that receives tree callbacks and pumps messages), because XAML objects may only be touched
/// from there: Lumen's commands arrive as window messages and run on this same thread.
fn create_control_window() {
    if WINDOW.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else { return };
        let class = WNDCLASSW { lpfnWndProc: Some(control_proc), hInstance: instance.into(), lpszClassName: w!("LumenTapWnd"), ..Default::default() };
        RegisterClassW(&class);
        match CreateWindowExW(WINDOW_EX_STYLE(0), w!("LumenTapWnd"), w!("Lumen taskbar"), WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, instance, None) {
            Ok(h) => {
                WINDOW.store(h.0 as isize, Ordering::SeqCst);
                log(&format!("control window ready on thread {}", windows::Win32::System::Threading::GetCurrentThreadId()));
            }
            Err(e) => log(&format!("control window failed: {e}")),
        }
    }
}

// ---- COM objects ---------------------------------------------------------------------

#[implement(IVisualTreeServiceCallback2)]
struct Watcher;

impl IVisualTreeServiceCallback_Impl for Watcher_Impl {
    unsafe fn OnVisualTreeChange(&self, relation: ParentChildRelation, element: VisualElement, mutation: i32) -> HRESULT {
        let r = catch_unwind(AssertUnwindSafe(|| {
            let handle = element.handle;
            create_control_window();
            if mutation == 0 {
                let ty = wide(element.type_name);
                let name = wide(element.name);
                let is_bg = (name == "BackgroundFill" || name == "BackgroundStroke") && ty.ends_with("Rectangle");
                let is_target = {
                    let mut t = tree();
                    t.parents.insert(relation.child, relation.parent);
                    t.types.insert(handle, ty);
                    let target = is_bg && under_taskbar(&t, handle);
                    if target {
                        t.targets.insert(handle, name == "BackgroundStroke");
                    }
                    target
                };
                if is_target {
                    log(&format!("found taskbar {name} ({handle}) on thread {}", windows::Win32::System::Threading::GetCurrentThreadId()));
                    if DESIRED.load(Ordering::SeqCst) >> 32 != 0 {
                        style(handle, name == "BackgroundStroke");
                    }
                }
            } else {
                let mut t = tree();
                t.parents.remove(&handle);
                t.types.remove(&handle);
                t.targets.remove(&handle);
            }
        }));
        if r.is_ok() { S_OK } else { E_FAIL }
    }
}

impl IVisualTreeServiceCallback2_Impl for Watcher_Impl {
    unsafe fn OnElementStateChanged(&self, _element: u64, _state: i32, _context: *const u16) -> HRESULT {
        S_OK
    }
}

#[implement(IObjectWithSite)]
struct Tap;

impl IObjectWithSite_Impl for Tap_Impl {
    fn SetSite(&self, site: Option<&IUnknown>) -> windows::core::Result<()> {
        let r = catch_unwind(AssertUnwindSafe(|| unsafe {
            let Some(site) = site else { return };
            log("SetSite");
            // Lumen passes the starting colour as "ARGB hex" in the initialization data.
            if let Ok(diag) = site.cast::<IXamlDiagnostics>() {
                let mut data: *const u16 = std::ptr::null();
                if diag.GetInitializationData(&mut data).is_ok() {
                    let text = wide(data);
                    if let Ok(argb) = u32::from_str_radix(text.trim(), 16) {
                        DESIRED.store((1u64 << 32) | argb as u64, Ordering::SeqCst);
                    }
                    if !data.is_null() {
                        drop(BSTR::from_raw(data));
                    }
                }
            }
            if let Ok(diag) = site.cast::<IXamlDiagnostics>() {
                let _ = DIAG.set(Diag(diag));
            }
            let Ok(service) = site.cast::<IVisualTreeService2>() else {
                log("no IVisualTreeService2");
                return;
            };
            let _ = SERVICE.set(Service(service));
            // Advise from a separate thread, as TranslucentTB does: the service then brings the
            // callbacks to the XAML UI thread itself, and calling it inline can hang.
            std::thread::spawn(|| {
                let _ = catch_unwind(|| {
                    if let Some(svc) = SERVICE.get() {
                        let watcher: IVisualTreeServiceCallback2 = Watcher.into();
                        let hr = unsafe { svc.0.AdviseVisualTreeChange(watcher.as_raw()) };
                        log(&format!("AdviseVisualTreeChange: {hr:?}"));
                        // The service keeps its own reference; ours stays alive for the process lifetime.
                        std::mem::forget(watcher);
                    }
                });
            });
        }));
        r.map_err(|_| E_FAIL.into())
    }

    fn GetSite(&self, _riid: *const GUID, ppv: *mut *mut c_void) -> windows::core::Result<()> {
        if !ppv.is_null() {
            unsafe { *ppv = std::ptr::null_mut() };
        }
        Err(E_FAIL.into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Option<&IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> windows::core::Result<()> {
        if outer.is_some() {
            return Err(windows::Win32::Foundation::CLASS_E_NOAGGREGATION.into());
        }
        let tap: IObjectWithSite = Tap.into();
        unsafe { tap.query(riid, ppv).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if *clsid != CLSID_TAP {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    log("DllGetClassObject");
    let factory: IClassFactory = Factory.into();
    factory.query(iid, ppv)
}

static MODULE: AtomicIsize = AtomicIsize::new(0);

/// The hook procedure Lumen installs; it only exists to get this DLL loaded.
#[no_mangle]
pub unsafe extern "system" fn CallWndProc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    CallNextHookEx(None, code, wp, lp)
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(module: windows::Win32::Foundation::HINSTANCE, reason: u32, _: *mut c_void) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0 as isize, Ordering::SeqCst);
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        log(&format!("loaded into {exe}"));
        if in_explorer() {
            std::thread::spawn(|| {
                let _ = catch_unwind(install);
            });
        }
    }
    true.into()
}

fn in_explorer() -> bool {
    std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).as_deref() == Some("explorer.exe")
}

/// Runs inside Explorer: connects XAML diagnostics with this DLL as the TAP, then signals
/// Lumen through the ready event either way.
fn install() {
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    type InitFn = unsafe extern "system" fn(PCWSTR, u32, PCWSTR, PCWSTR, GUID, PCWSTR) -> HRESULT;
    unsafe {
        let mut path = [0u16; 1024];
        let n = GetModuleFileNameW(windows::Win32::Foundation::HMODULE(MODULE.load(Ordering::SeqCst) as *mut _), &mut path) as usize;
        let mut dll: Vec<u16> = path[..n].to_vec();
        dll.push(0);
        let result = (|| {
            let xaml = LoadLibraryExW(w!("Windows.UI.Xaml.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let f = GetProcAddress(xaml, windows::core::s!("InitializeXamlDiagnosticsEx"))?;
            let init: InitFn = std::mem::transmute(f);
            let pid = std::process::id();
            let dll_ptr = dll.as_ptr() as usize;
            // Diagnostics can be initialized once per thread, so each attempt gets a new one.
            for attempt in 1..=10 {
                let hr = std::thread::spawn(move || {
                    let name: Vec<u16> = format!("VisualDiagConnection{attempt}").encode_utf16().chain(std::iter::once(0)).collect();
                    init(PCWSTR(name.as_ptr()), pid, PCWSTR::null(), PCWSTR(dll_ptr as *const u16), CLSID_TAP, PCWSTR::null()).0
                })
                .join()
                .unwrap_or(E_FAIL.0);
                log(&format!("InitializeXamlDiagnosticsEx attempt {attempt}: {:#010x}", hr as u32));
                if hr >= 0 {
                    return Some(());
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            None
        })();
        if result.is_none() {
            log("could not start XAML diagnostics");
        }
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\LumenTapReady")) {
            let _ = SetEvent(ev);
            let _ = windows::Win32::Foundation::CloseHandle(ev);
        }
    }
}

/// Stay loaded: Explorer holds callbacks into this DLL.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
