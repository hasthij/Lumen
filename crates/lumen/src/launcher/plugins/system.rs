//! System commands and Windows settings pages.

use crate::launcher::fuzzy;
use crate::launcher::item::{Action, Icon, Item, Kind, SystemCommand};

const COMMANDS: &[(&str, &str, &str, char, SystemCommand, bool)] = &[
    ("Lock", "Lock this PC", "lock screen", '\u{E72E}', SystemCommand::Lock, false),
    ("Sleep", "Put the PC to sleep", "suspend standby", '\u{E708}', SystemCommand::Sleep, false),
    ("Hibernate", "Hibernate the PC", "", '\u{E708}', SystemCommand::Hibernate, false),
    ("Shut down", "Turn off the PC", "shutdown power off turn off", '\u{E7E8}', SystemCommand::Shutdown, true),
    ("Restart", "Restart the PC", "reboot", '\u{E777}', SystemCommand::Restart, true),
    ("Sign out", "Sign out of Windows", "log off logout signout", '\u{F3B1}', SystemCommand::SignOut, true),
    ("Empty Recycle Bin", "Windows asks before deleting", "trash bin", '\u{E74D}', SystemCommand::EmptyRecycleBin, false),
];

pub fn commands(q: &str, explicit: bool, out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    if q.len() < 2 && !explicit {
        return;
    }
    for (title, sub, aliases, glyph, cmd, confirm) in COMMANDS {
        let s = fuzzy::score(&q, title).or_else(|| aliases.split(' ').filter_map(|a| fuzzy::score(&q, a)).max().map(|s| s - 100));
        let Some(s) = s.or(explicit.then_some(0)) else { continue };
        let mut item = Item::new(Kind::System, *title, *sub, Action::System(*cmd)).icon(Icon::Custom(*glyph)).score(s);
        if *confirm {
            item = item.confirm();
        }
        out.push(item);
    }
}

/// (name, target, extra search words)
const SETTINGS: &[(&str, &str, &str)] = &[
    ("Settings", "ms-settings:", "home"),
    ("Display", "ms-settings:display", "screen resolution brightness scale night light"),
    ("Night light", "ms-settings:nightlight", "blue light"),
    ("Sound", "ms-settings:sound", "audio volume speakers microphone"),
    ("Sound devices", "ms-settings:sound-devices", "audio output input"),
    ("Notifications", "ms-settings:notifications", "focus"),
    ("Power and battery", "ms-settings:powersleep", "sleep battery saver"),
    ("Storage", "ms-settings:storagesense", "disk space cleanup"),
    ("Multitasking", "ms-settings:multitasking", "snap windows"),
    ("Clipboard", "ms-settings:clipboard", "history"),
    ("About", "ms-settings:about", "pc name specs version"),
    ("Bluetooth and devices", "ms-settings:bluetooth", "devices pair"),
    ("Printers and scanners", "ms-settings:printers", ""),
    ("Mouse", "ms-settings:mousetouchpad", "pointer"),
    ("Touchpad", "ms-settings:devices-touchpad", "gestures"),
    ("Typing", "ms-settings:typing", "keyboard autocorrect"),
    ("AutoPlay", "ms-settings:autoplay", ""),
    ("Network and internet", "ms-settings:network-status", "wifi ethernet"),
    ("Wi-Fi", "ms-settings:network-wifi", "wireless"),
    ("VPN", "ms-settings:network-vpn", ""),
    ("Proxy", "ms-settings:network-proxy", ""),
    ("Personalization", "ms-settings:personalization", "theme"),
    ("Background", "ms-settings:personalization-background", "wallpaper"),
    ("Colors", "ms-settings:personalization-colors", "accent dark mode light mode"),
    ("Themes", "ms-settings:themes", ""),
    ("Lock screen", "ms-settings:lockscreen", ""),
    ("Start", "ms-settings:personalization-start", "start menu"),
    ("Taskbar", "ms-settings:taskbar", ""),
    ("Fonts", "ms-settings:fonts", ""),
    ("Installed apps", "ms-settings:appsfeatures", "uninstall programs"),
    ("Default apps", "ms-settings:defaultapps", "file associations"),
    ("Startup apps", "ms-settings:startupapps", "autostart"),
    ("Optional features", "ms-settings:optionalfeatures", ""),
    ("Accounts", "ms-settings:accounts", "your info"),
    ("Sign-in options", "ms-settings:signinoptions", "password pin windows hello"),
    ("Date and time", "ms-settings:dateandtime", "clock timezone"),
    ("Language and region", "ms-settings:regionlanguage", "locale"),
    ("Gaming", "ms-settings:gaming-gamebar", "game bar"),
    ("Game Mode", "ms-settings:gaming-gamemode", ""),
    ("Accessibility", "ms-settings:easeofaccess", "ease of access"),
    ("Privacy and security", "ms-settings:privacy", ""),
    ("Windows Security", "windowsdefender:", "antivirus defender firewall"),
    ("Windows Update", "ms-settings:windowsupdate", "updates"),
    ("Recovery", "ms-settings:recovery", "reset"),
    ("Activation", "ms-settings:activation", "product key"),
    ("For developers", "ms-settings:developers", "developer mode"),
    ("Graphics", "ms-settings:display-advancedgraphics", "gpu"),
    ("Control Panel", "control.exe", ""),
    ("Device Manager", "devmgmt.msc", "drivers hardware"),
    ("Disk Management", "diskmgmt.msc", "partitions"),
    ("Services", "services.msc", ""),
    ("Event Viewer", "eventvwr.msc", "logs"),
    ("Task Manager", "taskmgr.exe", "processes"),
    ("Registry Editor", "regedit.exe", ""),
    ("System Properties", "sysdm.cpl", ""),
    ("Environment variables", "rundll32.exe|sysdm.cpl,EditEnvironmentVariables", "path"),
    ("Programs and Features", "appwiz.cpl", "uninstall"),
    ("Network Connections", "ncpa.cpl", "adapters"),
    ("Sound Control Panel", "mmsys.cpl", "playback recording"),
    ("Power Options", "powercfg.cpl", "power plan"),
    ("Computer Management", "compmgmt.msc", ""),
    ("Task Scheduler", "taskschd.msc", ""),
];

pub fn settings(q: &str, explicit: bool, out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    if q.is_empty() && !explicit {
        return;
    }
    for (name, target, words) in SETTINGS {
        let s = if q.is_empty() {
            Some(0)
        } else {
            fuzzy::score(&q, name).or_else(|| words.split(' ').filter_map(|w| fuzzy::score(&q, w)).max().map(|s| s - 200))
        };
        let Some(s) = s else { continue };
        let (file, args) = target.split_once('|').unwrap_or((target, ""));
        let sub = if file.starts_with("ms-settings") { "Settings page" } else { "Control panel" };
        out.push(
            Item::new(Kind::Settings, *name, sub, Action::Open { target: file.into(), args: args.into() })
                .key(format!("settings:{target}"))
                .score(s - 50),
        );
    }
}
