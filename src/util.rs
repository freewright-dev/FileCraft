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
    let offset = local_offset();
    let local_now = now_filetime() + offset;
    local_now - local_now.rem_euclid(TICKS_PER_DAY) - offset
}

/// FILETIME of local midnight for a calendar date.
pub fn local_midnight(year: i64, month: u32, day: u32) -> i64 {
    let days = days_from_civil(year, month, day);
    UNIX_EPOCH_TICKS + days * TICKS_PER_DAY - local_offset()
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
    let local = ft + offset - UNIX_EPOCH_TICKS;
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

#[cfg(test)]
mod tests {
    use super::*;

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
