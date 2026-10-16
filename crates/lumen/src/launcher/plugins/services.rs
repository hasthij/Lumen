//! Windows services: find a service and start, stop or restart it (asks for admin).

use windows::core::PWSTR;
use windows::Win32::System::Services::*;

use crate::launcher::fuzzy;
use crate::launcher::item::{Action, Icon, Item, Kind};

struct Service {
    name: String,
    display: String,
    running: bool,
}

fn list() -> Vec<Service> {
    let mut out = Vec::new();
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) else { return out };
        let mut needed = 0u32;
        let mut count = 0u32;
        let mut resume = 0u32;
        let _ = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL, None, &mut needed, &mut count, Some(&mut resume), None);
        let mut buf = vec![0u8; needed as usize + 4096];
        resume = 0;
        if EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL, Some(&mut buf), &mut needed, &mut count, Some(&mut resume), None).is_ok() {
            let entries = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
            for e in entries {
                let s = |p: PWSTR| p.to_string().unwrap_or_default();
                out.push(Service {
                    name: s(e.lpServiceName),
                    display: s(e.lpDisplayName),
                    running: e.ServiceStatusProcess.dwCurrentState == SERVICE_RUNNING,
                });
            }
        }
        let _ = CloseServiceHandle(scm);
    }
    out
}

pub fn query(q: &str, out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    for s in list() {
        let score = if q.is_empty() { Some(0) } else { fuzzy::score(&q, &s.display).max(fuzzy::score(&q, &s.name)) };
        let Some(score) = score else { continue };
        let net = |verb: &str, label: &'static str, glyph: char| Action::Elevated {
            target: "cmd.exe".into(),
            args: format!("/c net {verb} \"{}\"", s.name),
            label,
            glyph,
        };
        let (state, primary) = if s.running {
            ("Running · Enter to stop", net("stop", "Stop service", '\u{E71A}'))
        } else {
            ("Stopped · Enter to start", net("start", "Start service", '\u{E768}'))
        };
        let restart = Action::Elevated {
            target: "cmd.exe".into(),
            args: format!("/c net stop \"{0}\" & net start \"{0}\"", s.name),
            label: "Restart service",
            glyph: '\u{E72C}',
        };
        out.push(
            Item::new(Kind::Service, s.display.clone(), format!("{state}  ·  {}", s.name), primary)
                .icon(Icon::Custom(if s.running { '\u{E768}' } else { '\u{E71A}' }))
                .key(format!("service:{}", s.name))
                .score(score)
                .with(restart)
                .with(Action::Copy(s.name.clone())),
        );
    }
}
