//! Time and date: "time", "date", "now", "week", "unix", and a unix timestamp → date.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimeAsFileTime};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::launcher::item::{Action, Item, Kind};
use crate::rainmeter::time::format;

const WORDS: &[&str] = &["time", "date", "now", "week", "unix", "utc", "today", "clock"];

pub fn query(q: &str, explicit: bool, out: &mut Vec<Item>) {
    let q = q.trim().to_lowercase();
    if !explicit && (q.len() < 3 || !WORDS.iter().any(|w| w.starts_with(&q))) {
        return;
    }
    if let Ok(ts) = q.parse::<i64>() {
        if let Some(t) = from_unix(ts) {
            push(out, format(r"%A, %d %B %Y  %H:%M:%S", &t), "Local time of that Unix timestamp", 4000);
        }
        return;
    }
    let now = unsafe { GetLocalTime() };
    let unix = unix_now();
    let score = if explicit { 4000 } else { 1500 };
    let mut all = vec![
        (format("%H:%M:%S", &now), "Time", "time clock now"),
        (format("%I:%M %p", &now), "Time (12-hour)", "time clock now"),
        (format("%A, %d %B %Y", &now), "Date", "date today now"),
        (format("%Y-%m-%d", &now), "Date (ISO)", "date today now"),
        (format!("Week {}", iso_week(&now)), "Week number", "week date"),
        (unix.to_string(), "Unix timestamp", "unix now utc"),
    ];
    if !explicit || !q.is_empty() {
        all.retain(|(_, _, tags)| tags.split(' ').any(|t| t.starts_with(q.as_str())));
    }
    for (i, (value, what, _)) in all.into_iter().enumerate() {
        push(out, value, what, score - i as i32);
    }
}

fn push(out: &mut Vec<Item>, value: String, what: &str, score: i32) {
    out.push(Item::new(Kind::Time, value.clone(), format!("{what}  ·  Enter to copy"), Action::Copy(value)).key(format!("time:{what}")).score(score));
}

fn unix_now() -> i64 {
    let ft = unsafe { GetSystemTimeAsFileTime() };
    let t = (ft.dwHighDateTime as i64) << 32 | ft.dwLowDateTime as i64;
    (t - 116_444_736_000_000_000) / 10_000_000
}

fn from_unix(ts: i64) -> Option<SYSTEMTIME> {
    // Accept seconds or milliseconds.
    let secs = if ts > 100_000_000_000 { ts / 1000 } else { ts };
    let t = secs.checked_mul(10_000_000)?.checked_add(116_444_736_000_000_000)?;
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    unsafe {
        let mut utc = SYSTEMTIME::default();
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        let mut local = SYSTEMTIME::default();
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        Some(local)
    }
}

/// ISO 8601 week number.
fn iso_week(t: &SYSTEMTIME) -> u32 {
    let days_before = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let y = t.wYear as i32;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut ordinal = days_before[(t.wMonth as usize).clamp(1, 12) - 1] + t.wDay as i32;
    if leap && t.wMonth > 2 {
        ordinal += 1;
    }
    let weekday = if t.wDayOfWeek == 0 { 7 } else { t.wDayOfWeek as i32 };
    let week = (ordinal - weekday + 10) / 7;
    week.clamp(1, 53) as u32
}
