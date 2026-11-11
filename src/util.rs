//! Formatting and time helpers. Times are Windows FILETIMEs: 100 ns ticks since 1601-01-01 UTC.

pub const TICKS_PER_SEC: i64 = 10_000_000;
pub const TICKS_PER_DAY: i64 = 86_400 * TICKS_PER_SEC;
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

pub fn now_filetime() -> i64 {
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_EPOCH_TICKS + (since_unix.as_nanos() / 100) as i64
}

/// Offset of local time from UTC, in ticks (uses the current daylight-saving state).
#[cfg(windows)]
pub fn local_offset() -> i64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    let now = now_filetime();
    let utc = FILETIME { dwLowDateTime: now as u32, dwHighDateTime: (now >> 32) as u32 };
    let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: both pointers reference valid FILETIME structs.
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
        return 0;
    }
    (((local.dwHighDateTime as i64) << 32) | local.dwLowDateTime as i64) - now
}

#[cfg(not(windows))]
pub fn local_offset() -> i64 {
    0
}

/// FILETIME of local midnight at the start of today.
pub fn local_midnight_today() -> i64 {
    let local_now = now_filetime() + local_offset();
    let (y, m, d) = civil_from_days((local_now - UNIX_EPOCH_TICKS).div_euclid(TICKS_PER_DAY));
    local_midnight(y, m, d)
}

/// FILETIME of local midnight for a calendar date, using the UTC offset in effect on that date
/// rather than today's, so a date on the other side of a daylight-saving change is not an hour
/// off. Falls back to today's offset if Windows cannot convert the date.
#[cfg(windows)]
pub fn local_midnight(year: i64, month: u32, day: u32) -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation, TIME_ZONE_ID_INVALID, TzSpecificLocalTimeToSystemTimeEx,
    };
    let fallback = || UNIX_EPOCH_TICKS + days_from_civil(year, month, day) * TICKS_PER_DAY - local_offset();
    let (Ok(year), Ok(month), Ok(day)) = (u16::try_from(year), u16::try_from(month), u16::try_from(day)) else {
        return fallback();
    };
    let local = SYSTEMTIME { wYear: year, wMonth: month, wDay: day, ..SYSTEMTIME::default() };
    let mut utc = SYSTEMTIME::default();
    let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: every pointer references a valid, writable-where-needed struct of the right type.
    let converted = unsafe {
        GetDynamicTimeZoneInformation(&mut zone) != TIME_ZONE_ID_INVALID
            && TzSpecificLocalTimeToSystemTimeEx(&zone, &local, &mut utc) != 0
    };
    if !converted {
        return fallback();
    }
    let days = days_from_civil(utc.wYear as i64, utc.wMonth as u32, utc.wDay as u32);
    let secs = (utc.wHour as i64 * 60 + utc.wMinute as i64) * 60 + utc.wSecond as i64;
    UNIX_EPOCH_TICKS + days * TICKS_PER_DAY + secs * TICKS_PER_SEC + utc.wMilliseconds as i64 * 10_000
}

#[cfg(not(windows))]
pub fn local_midnight(year: i64, month: u32, day: u32) -> i64 {
    UNIX_EPOCH_TICKS + days_from_civil(year, month, day) * TICKS_PER_DAY
}

/// Parses `YYYY-MM-DD` (or `YYYY/MM/DD`).
pub fn parse_date(s: &str) -> Option<(i64, u32, u32)> {
    let mut parts = s.split(['-', '/', '.']);
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1601..=9999).contains(&y) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's days-from-civil (days since 1970-01-01).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// `2026-10-13 14:05` in local time; empty for unknown times.
pub fn format_filetime(ft: i64, offset: i64) -> String {
    if ft <= 0 {
        return String::new();
    }
    // Saturating: timestamps and offsets come from the file system and time zone settings, so
    // extreme values must not overflow (debug panic, FC-19).
    let local = ft.saturating_add(offset).saturating_sub(UNIX_EPOCH_TICKS);
    let days = local.div_euclid(TICKS_PER_DAY);
    let secs = local.rem_euclid(TICKS_PER_DAY) / TICKS_PER_SEC;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `1234567` → `1,234,567`.
pub fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The path to use for file-system calls on an indexed item. Index paths are plain (`X:\a\b`),
/// and Win32 normalises plain paths: it strips trailing dots and spaces from components, so
/// `X:\report.txt.` would open `X:\report.txt`. The verbatim form (`\\?\X:\…`) reaches the
/// named item itself (FC-15). Paths that are not drive-absolute are returned unchanged.
pub fn fs_path(full: &str) -> std::path::PathBuf {
    let b = full.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        format!(r"\\?\{full}").into()
    } else {
        full.into()
    }
}

/// Whether Win32 would resolve the plain path `full` to a different item: a component ends in a
/// dot or a space (which covers `.` and `..`), or is a reserved device name (`CON`, `NUL`,
/// `CONIN$`, `COM1`, `COM¹`, `LPT1.txt` …). Both `\` and `/` separate components. The shell and
/// other programs only accept plain paths, so such items cannot be handed to them safely.
pub fn win32_alias(full: &str) -> bool {
    const DEVICES: [&str; 7] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$"];
    let rest = full.get(3..).filter(|_| full.as_bytes().get(1) == Some(&b':')).unwrap_or(full);
    rest.split(['\\', '/']).filter(|c| !c.is_empty()).any(|c| {
        if c.ends_with(['.', ' ']) {
            return true;
        }
        let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
        let upper = stem.to_ascii_uppercase();
        // `COM1`-`COM9` and `LPT1`-`LPT9`; Win32 also reads the superscripts `¹²³` as digits.
        let numbered = upper.strip_prefix("COM").or_else(|| upper.strip_prefix("LPT")).is_some_and(|n| {
            matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        });
        DEVICES.contains(&upper.as_str()) || numbered
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FC-15: names Win32 would rewrite are detected, and file-system calls use verbatim paths.
    #[test]
    fn detects_paths_win32_would_rewrite() {
        for alias in [r"C:\x\report.txt.", r"C:\x\space.txt ", r"C:\sub.\f.txt", r"C:\x\CON", r"C:\x\nul.txt", r"C:\x\com1"] {
            assert!(win32_alias(alias), "{alias}");
        }
        for plain in [r"C:\", r"C:\x\report.txt", r"C:\.git\config", r"C:\x\console.txt", r"C:\x\COM0", r"C:\x\a b\c"] {
            assert!(!win32_alias(plain), "{plain}");
        }
        #[cfg(windows)]
        assert_eq!(fs_path(r"C:\x\report.txt."), std::path::PathBuf::from(r"\\?\C:\x\report.txt."));
        assert_eq!(fs_path("relative"), std::path::PathBuf::from("relative"));
    }

    /// Device names Win32 also reserves (`CONIN$`, `COM¹`), and components it rewrites when they
    /// are separated by `/`, were not detected, so the shell could be handed a different item.
    #[test]
    fn detects_more_paths_win32_would_rewrite() {
        for alias in [
            r"C:\x\..\Windows\win.ini",
            r"C:\x\.\f.txt",
            r"C:\x\CONIN$",
            r"C:\x\conout$.txt",
            r"C:\x\CLOCK$",
            r"C:\x\COM¹",
            r"C:\x\lpt³.log",
            "C:/x/report.txt.",
            "C:/x/nul",
            "C:/x/../y",
        ] {
            assert!(win32_alias(alias), "{alias}");
        }
        for plain in [r"C:\x\COM⁴", r"C:\x\COM10", r"C:\x\CONIN", r"C:\x\com¹x", "C:/x/report.txt"] {
            assert!(!win32_alias(plain), "{plain}");
        }
    }

    /// Date filters used today's UTC offset for every date, so a date on the other side of a
    /// daylight-saving change started an hour early or late. Midnight of a winter and of a
    /// summer date must each read back as 00:00 local time on that date.
    #[cfg(windows)]
    #[test]
    fn local_midnight_uses_the_dates_own_offset() {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::Time::{
            DYNAMIC_TIME_ZONE_INFORMATION, FileTimeToSystemTime, GetDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTimeEx,
        };
        let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // SAFETY: `zone` is a valid, writable struct.
        unsafe { GetDynamicTimeZoneInformation(&mut zone) };
        for (y, m, d) in [(2026, 1, 15), (2026, 7, 15), (2025, 12, 31), (2025, 6, 30)] {
            let ft = local_midnight(y, m, d);
            let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
            let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
            // SAFETY: every pointer references a valid struct of the right type.
            let ok = unsafe { FileTimeToSystemTime(&ft, &mut utc) != 0 && SystemTimeToTzSpecificLocalTimeEx(&zone, &utc, &mut local) != 0 };
            assert!(ok, "{y}-{m}-{d}");
            let got = (local.wYear as i64, local.wMonth as u32, local.wDay as u32, local.wHour, local.wMinute, local.wSecond);
            assert_eq!(got, (y, m, d, 0, 0, 0), "{y}-{m}-{d}");
        }
    }

    /// FC-19: a timestamp near `i64::MAX` (settable by any user) panicked in debug builds when a
    /// positive offset was added to it. Formatting must saturate instead.
    #[test]
    fn extreme_timestamps_do_not_overflow() {
        let _ = format_filetime(i64::MAX, 264_436_543_536);
        let _ = format_filetime(9_223_371_772_418_232_272, 264_436_543_536);
        let _ = format_filetime(i64::MAX, i64::MAX);
        let _ = format_filetime(1, i64::MIN);
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(250 * 1024 * 1024), "250 MB");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(group_digits(12), "12");
        // 2026-10-13 14:05 UTC
        let ft = UNIX_EPOCH_TICKS + (days_from_civil(2026, 10, 13) * 86_400 + 14 * 3600 + 5 * 60) * TICKS_PER_SEC;
        assert_eq!(format_filetime(ft, 0), "2026-10-13 14:05");
        assert_eq!(format_filetime(0, 0), "");
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_date("2024-13-01"), None);
        // MIN-02: impossible days used to roll over into the next month.
        assert_eq!(parse_date("2026-02-31"), None);
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date("2026-04-31"), None);
        assert_eq!(parse_date("2026-12-31"), Some((2026, 12, 31)));
        assert_eq!(parse_date("1900-02-29"), None);
        assert_eq!(parse_date("2000-02-29"), Some((2000, 2, 29)));
        assert_eq!(parse_date("2026-01-00"), None);
    }
}
