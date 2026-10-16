//! strftime subset used by Rainmeter Time measures (C locale, like Rainmeter's default).

use windows::Win32::Foundation::SYSTEMTIME;

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

pub fn format(fmt: &str, t: &SYSTEMTIME) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // `%#d` drops leading zeros, as in MSVC's strftime.
        let trim = chars.peek() == Some(&'#');
        if trim {
            chars.next();
        }
        let Some(spec) = chars.next() else { break };
        let day = DAYS[(t.wDayOfWeek as usize).min(6)];
        let month = MONTHS[(t.wMonth as usize).clamp(1, 12) - 1];
        let h12 = match t.wHour % 12 {
            0 => 12,
            h => h,
        };
        let num = |n: u16| if trim { n.to_string() } else { format!("{n:02}") };
        match spec {
            'a' => out.push_str(&day[..3]),
            'A' => out.push_str(day),
            'b' | 'h' => out.push_str(&month[..3]),
            'B' => out.push_str(month),
            'd' => out.push_str(&num(t.wDay)),
            'H' => out.push_str(&num(t.wHour)),
            'I' => out.push_str(&num(h12)),
            'M' => out.push_str(&num(t.wMinute)),
            'S' => out.push_str(&num(t.wSecond)),
            'm' => out.push_str(&num(t.wMonth)),
            'p' => out.push_str(if t.wHour < 12 { "AM" } else { "PM" }),
            'y' => out.push_str(&num(t.wYear % 100)),
            'Y' => out.push_str(&t.wYear.to_string()),
            'w' => out.push_str(&t.wDayOfWeek.to_string()),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

/// True when the format changes more often than once a minute.
pub fn needs_seconds(fmt: &str) -> bool {
    fmt.contains("%S") || fmt.contains("%#S") || fmt.contains("%c") || fmt.contains("%X") || fmt.contains("%T")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let t = SYSTEMTIME { wYear: 2026, wMonth: 10, wDayOfWeek: 5, wDay: 16, wHour: 17, wMinute: 5, wSecond: 9, wMilliseconds: 0 };
        assert_eq!(format("%I:%M", &t), "05:05");
        assert_eq!(format("%p", &t), "PM");
        assert_eq!(format("%A", &t), "Friday");
        assert_eq!(format("%d %B", &t), "16 October");
        assert_eq!(format("%H", &t), "17");
        assert_eq!(format("%#I", &t), "5");
        let midnight = SYSTEMTIME { wHour: 0, ..t };
        assert_eq!(format("%I %p", &midnight), "12 AM");
    }
}
