//! Volume mixer: master endpoint + per-app audio sessions, and per-monitor brightness
//! (DDC/CI for external screens, WMI for laptop panels). All calls need COM on the calling thread.

use std::sync::Mutex;

use windows::core::{w, Interface, BSTR, PWSTR, VARIANT};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT, S_OK};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, MonitorFromWindow, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eMultimedia, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoSetProxyBlanket, CLSCTX_ALL, CLSCTX_INPROC_SERVER, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::Wmi::{IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE};

/// One app in the mixer. `key` is the lowercase exe stem; all its sessions move together.
#[derive(Clone, PartialEq)]
pub struct Chan {
    pub key: String,
    pub name: String,
    pub path: String,
    pub vol: f32,
    pub mute: bool,
}

unsafe fn device() -> Option<IMMDevice> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()
}

unsafe fn endpoint() -> Option<IAudioEndpointVolume> {
    device()?.Activate(CLSCTX_ALL, None).ok()
}

/// Master volume 0..1 and mute.
pub fn master() -> Option<(f32, bool)> {
    unsafe {
        let ep = endpoint()?;
        Some((ep.GetMasterVolumeLevelScalar().ok()?, ep.GetMute().ok()?.as_bool()))
    }
}

pub fn set_master(v: Option<f32>, mute: Option<bool>) {
    unsafe {
        let Some(ep) = endpoint() else { return };
        if let Some(v) = v {
            let _ = ep.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null());
        }
        if let Some(m) = mute {
            let _ = ep.SetMute(m, std::ptr::null());
        }
    }
}

/// Calls `f(pid, active, volume)` for every non-system app session.
unsafe fn each(mut f: impl FnMut(u32, bool, &ISimpleAudioVolume)) -> Option<()> {
    let mgr: IAudioSessionManager2 = device()?.Activate(CLSCTX_ALL, None).ok()?;
    let list = mgr.GetSessionEnumerator().ok()?;
    for i in 0..list.GetCount().ok()? {
        let Ok(s) = list.GetSession(i) else { continue };
        let active = s.GetState().ok() == Some(AudioSessionStateActive);
        let Ok(s2) = s.cast::<IAudioSessionControl2>() else { continue };
        if s2.IsSystemSoundsSession() == S_OK {
            continue;
        }
        let pid = s2.GetProcessId().unwrap_or(0);
        if pid == 0 {
            continue;
        }
        if let Ok(v) = s.cast::<ISimpleAudioVolume>() {
            f(pid, active, &v);
        }
    }
    Some(())
}

fn key_of(path: &str) -> String {
    std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn pretty(key: &str) -> String {
    let mut c = key.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Apps playing audio now, plus pinned apps that have a session (playing or not).
/// Pinned apps come first in pin order, the rest sorted by name.
pub fn apps(pins: &[String]) -> Vec<Chan> {
    let mut out: Vec<Chan> = Vec::new();
    let mut seen: Vec<(u32, String)> = Vec::new();
    unsafe {
        each(|pid, active, v| {
            let key = match seen.iter().find(|s| s.0 == pid) {
                Some(s) => s.1.clone(),
                None => {
                    let path = exe(pid).unwrap_or_default();
                    let k = key_of(&path);
                    seen.push((pid, k.clone()));
                    if !k.is_empty() && !out.iter().any(|c| c.key == k) && (active || pins.contains(&k)) {
                        out.push(Chan { name: pretty(&k), key: k.clone(), path, vol: v.GetMasterVolume().unwrap_or(1.0), mute: v.GetMute().map(|b| b.as_bool()).unwrap_or(false) });
                    }
                    return;
                }
            };
            // A later playing session of an already-seen process can still add it.
            if active && !key.is_empty() && !out.iter().any(|c| c.key == key) {
                let path = exe(pid).unwrap_or_default();
                out.push(Chan { name: pretty(&key), key, path, vol: v.GetMasterVolume().unwrap_or(1.0), mute: v.GetMute().map(|b| b.as_bool()).unwrap_or(false) });
            }
        });
    }
    let rank = |c: &Chan| pins.iter().position(|p| *p == c.key).unwrap_or(usize::MAX);
    out.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.key.cmp(&b.key)));
    out
}

/// Every app with an audio session (any state), as (name, key): candidates for pinning.
pub fn sessions() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut pids = Vec::new();
    unsafe {
        each(|pid, _, _| {
            if pids.contains(&pid) {
                return;
            }
            pids.push(pid);
            let k = key_of(&exe(pid).unwrap_or_default());
            if !k.is_empty() && !out.iter().any(|o| o.1 == k) {
                out.push((pretty(&k), k));
            }
        });
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Sets volume/mute on every session belonging to the app `key`.
pub fn set_app(key: &str, v: Option<f32>, mute: Option<bool>) {
    let mut keys: Vec<(u32, bool)> = Vec::new();
    unsafe {
        each(|pid, _, s| {
            let hit = match keys.iter().find(|k| k.0 == pid) {
                Some(k) => k.1,
                None => {
                    let h = key_of(&exe(pid).unwrap_or_default()) == key;
                    keys.push((pid, h));
                    h
                }
            };
            if !hit {
                return;
            }
            if let Some(v) = v {
                let _ = s.SetMasterVolume(v.clamp(0.0, 1.0), std::ptr::null());
            }
            if let Some(m) = mute {
                let _ = s.SetMute(m, std::ptr::null());
            }
        });
    }
}

unsafe fn exe(pid: u32) -> Option<String> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 512];
    let mut n = buf.len() as u32;
    let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
    let _ = CloseHandle(h);
    r.ok()?;
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

// ---- brightness -------------------------------------------------------------------------

/// Which monitor (by index) is driven through WMI, learned by `brightness_all`.
static WMI_MON: Mutex<Option<usize>> = Mutex::new(None);

/// Monitors left to right.
fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, r: *mut RECT, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<(i32, HMONITOR)>);
        v.push(((*r).left, h));
        true.into()
    }
    let mut v: Vec<(i32, HMONITOR)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.sort_by_key(|m| m.0);
    v.into_iter().map(|m| m.1).collect()
}

/// Index of the monitor under `hwnd`.
pub fn monitor_of(hwnd: HWND) -> usize {
    let h = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    monitors().iter().position(|m| *m == h).unwrap_or(0)
}

/// DDC/CI: reads (and if `set`, writes) brightness 0..1.
unsafe fn ddc(h: HMONITOR, set: Option<f32>) -> Option<f32> {
    use windows::Win32::Devices::Display::{DestroyPhysicalMonitors, GetMonitorBrightness, GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR};
    let mut m = [PHYSICAL_MONITOR::default()];
    GetPhysicalMonitorsFromHMONITOR(h, &mut m).ok()?;
    let (mut lo, mut cur, mut hi) = (0u32, 0u32, 0u32);
    let mut out = None;
    if GetMonitorBrightness(m[0].hPhysicalMonitor, &mut lo, &mut cur, &mut hi) != 0 && hi > lo {
        out = Some((cur.saturating_sub(lo)) as f32 / (hi - lo) as f32);
        if let Some(v) = set {
            SetMonitorBrightness(m[0].hPhysicalMonitor, lo + (v.clamp(0.0, 1.0) * (hi - lo) as f32).round() as u32);
            out = Some(v.clamp(0.0, 1.0));
        }
    }
    let _ = DestroyPhysicalMonitors(&m);
    out
}

unsafe fn wmi() -> Option<IWbemServices> {
    let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
    let svc = loc.ConnectServer(&BSTR::from(r"ROOT\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None).ok()?;
    let _ = CoSetProxyBlanket(&svc, 10, 0, None, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE);
    Some(svc)
}

unsafe fn wmi_first(svc: &IWbemServices, class: &str) -> Option<IWbemClassObject> {
    let flags = WBEM_GENERIC_FLAG_TYPE(WBEM_FLAG_FORWARD_ONLY.0 | WBEM_FLAG_RETURN_IMMEDIATELY.0);
    let en = svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(format!("SELECT * FROM {class}")), flags, None).ok()?;
    let mut row = [None];
    let mut n = 0u32;
    let _ = en.Next(-1, &mut row, &mut n);
    row[0].take()
}

/// Laptop panel via WMI: reads (and if `set`, writes) brightness 0..1.
unsafe fn wmi_bri(set: Option<f32>) -> Option<f32> {
    let svc = wmi()?;
    if let Some(v) = set {
        let v = v.clamp(0.0, 1.0);
        let inst = wmi_first(&svc, "WmiMonitorBrightnessMethods")?;
        let mut path = VARIANT::default();
        inst.Get(w!("__PATH"), 0, &mut path, None, None).ok()?;
        let path = BSTR::try_from(&path).ok()?;
        let mut class: Option<IWbemClassObject> = None;
        svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None).ok()?;
        let mut sig: Option<IWbemClassObject> = None;
        class?.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, std::ptr::null_mut()).ok()?;
        let args = sig?.SpawnInstance(0).ok()?;
        args.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0).ok()?;
        args.Put(w!("Brightness"), 0, &VARIANT::from((v * 100.0).round() as u8), 0).ok()?;
        svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &args, None, None).ok()?;
        return Some(v);
    }
    let obj = wmi_first(&svc, "WmiMonitorBrightness")?;
    let mut v = VARIANT::default();
    obj.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
    Some(i32::try_from(&v).ok()? as f32 / 100.0)
}

/// Brightness of every monitor, left to right; None where it can't be controlled.
/// The first monitor without DDC/CI is assumed to be the laptop panel and tried via WMI.
pub fn brightness_all() -> Vec<Option<f32>> {
    let mut wmi_mon = None;
    let out = monitors()
        .into_iter()
        .enumerate()
        .map(|(i, h)| unsafe {
            ddc(h, None).or_else(|| {
                if wmi_mon.is_some() {
                    return None;
                }
                let r = wmi_bri(None);
                if r.is_some() {
                    wmi_mon = Some(i);
                }
                r
            })
        })
        .collect();
    if let Ok(mut m) = WMI_MON.lock() {
        *m = wmi_mon;
    }
    out
}

/// Sets monitor `i` to `v` (0..1), or reads it when `v` is None. Slow: call off the UI thread.
pub fn brightness(i: usize, v: Option<f32>) -> Option<f32> {
    let h = *monitors().get(i)?;
    unsafe {
        if WMI_MON.lock().ok().and_then(|m| *m) == Some(i) {
            return wmi_bri(v);
        }
        ddc(h, v).or_else(|| {
            // Not probed yet (e.g. wheel HUD before the Sound tab was opened).
            let mut m = WMI_MON.lock().ok()?;
            if m.is_none() {
                let r = wmi_bri(v);
                if r.is_some() {
                    *m = Some(i);
                }
                return r;
            }
            None
        })
    }
}
