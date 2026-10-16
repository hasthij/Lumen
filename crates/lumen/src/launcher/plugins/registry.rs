//! Registry browser: ": hklm\software\micro" lists matching subkeys and the key's values.
//! Enter opens Registry Editor at the key.

use windows::core::{HSTRING, PWSTR};
use windows::Win32::System::Registry::*;

use crate::launcher::fuzzy;
use crate::launcher::item::{Action, Icon, Item, Kind};

const ROOTS: &[(&str, &str, HKEY)] = &[
    ("HKEY_CLASSES_ROOT", "HKCR", HKEY_CLASSES_ROOT),
    ("HKEY_CURRENT_USER", "HKCU", HKEY_CURRENT_USER),
    ("HKEY_LOCAL_MACHINE", "HKLM", HKEY_LOCAL_MACHINE),
    ("HKEY_USERS", "HKU", HKEY_USERS),
    ("HKEY_CURRENT_CONFIG", "HKCC", HKEY_CURRENT_CONFIG),
];

pub fn query(q: &str, out: &mut Vec<Item>) {
    let q = q.trim().trim_start_matches("Computer\\").replace('/', "\\");
    let (root_part, rest) = q.split_once('\\').unwrap_or((q.as_str(), ""));
    let root = ROOTS.iter().find(|(full, short, _)| full.eq_ignore_ascii_case(root_part) || short.eq_ignore_ascii_case(root_part));
    let Some(&(full, _, hroot)) = root.filter(|_| q.contains('\\')) else {
        // Still typing the hive name.
        let ql = q.to_lowercase();
        for (full, short, _) in ROOTS {
            if ql.is_empty() || full.to_lowercase().starts_with(&ql) || short.to_lowercase().starts_with(&ql) {
                out.push(key_item(full, &format!("{short}  ·  Enter to open"), 1000));
            }
        }
        return;
    };

    // "parent\partial": list children of parent that match partial.
    let (parent, filter) = match rest.rfind('\\') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => ("", rest),
    };
    let filter = filter.to_lowercase();
    let parent_path = if parent.is_empty() { full.to_string() } else { format!("{full}\\{parent}") };
    let Some(key) = open(hroot, parent) else { return };

    if filter.is_empty() {
        out.push(key_item(&parent_path, "Open this key", 3000));
    }
    for (i, name) in subkeys(key).into_iter().enumerate() {
        let s = if filter.is_empty() { Some(2000 - i as i32) } else { fuzzy::score(&filter, &name) };
        if let Some(s) = s {
            out.push(key_item(&format!("{parent_path}\\{name}"), "Key", s));
        }
        if out.len() > 200 {
            break;
        }
    }
    if filter.is_empty() {
        for (name, data) in values(key).into_iter().take(50) {
            let shown = if name.is_empty() { "(Default)".to_string() } else { name };
            out.push(
                Item::new(Kind::Registry, format!("{shown} = {data}"), format!("Value in {parent_path}  ·  Enter to copy"), Action::Copy(data))
                    .icon(Icon::Custom('\u{E8AC}'))
                    .score(500),
            );
        }
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
}

fn key_item(path: &str, sub: &str, score: i32) -> Item {
    Item::new(Kind::Registry, path, sub, Action::RegistryOpen(path.to_string()))
        .key(format!("registry:{path}"))
        .score(score)
        .with(Action::Copy(path.to_string()))
}

fn open(root: HKEY, sub: &str) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(root, &HSTRING::from(sub), 0, KEY_READ, &mut key).is_ok().then_some(key) }
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = [0u16; 256];
    for i in 0.. {
        let mut len = buf.len() as u32;
        let r = unsafe { RegEnumKeyExW(key, i, PWSTR(buf.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None) };
        if r.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

fn values(key: HKEY) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut name = [0u16; 512];
    let mut data = vec![0u8; 4096];
    for i in 0.. {
        let mut nlen = name.len() as u32;
        let mut dlen = data.len() as u32;
        let mut ty = 0u32;
        let r = unsafe {
            RegEnumValueW(key, i, PWSTR(name.as_mut_ptr()), &mut nlen, None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut dlen))
        };
        if r.is_err() {
            // ERROR_MORE_DATA on one big value shouldn't hide the rest.
            if r.0 == 234 {
                continue;
            }
            break;
        }
        let n = String::from_utf16_lossy(&name[..nlen as usize]);
        let d = &data[..dlen as usize];
        let text = match REG_VALUE_TYPE(ty) {
            REG_SZ | REG_EXPAND_SZ | REG_MULTI_SZ => {
                let wide: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                String::from_utf16_lossy(&wide).trim_end_matches('\0').replace('\0', "; ")
            }
            REG_DWORD if d.len() >= 4 => {
                let v = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                format!("0x{v:08x} ({v})")
            }
            REG_QWORD if d.len() >= 8 => {
                let v = u64::from_le_bytes(d[..8].try_into().unwrap_or_default());
                format!("0x{v:016x} ({v})")
            }
            _ => d.iter().take(32).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
        };
        out.push((n, text));
    }
    out
}
