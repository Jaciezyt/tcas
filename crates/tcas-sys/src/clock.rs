//! Wall-clock time in milliseconds and local-time formatting.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn local(ms: i64) -> libc::tm {
    let t: libc::time_t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes into the zeroed struct we pass and returns it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// `14:02`
pub fn hhmm(ms: i64) -> String {
    let tm = local(ms);
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// `14:02` today, `11-20 14:02` on other days.
pub fn short(ms: i64, now: i64) -> String {
    let (a, b) = (local(ms), local(now));
    if a.tm_year == b.tm_year && a.tm_yday == b.tm_yday {
        hhmm(ms)
    } else {
        format!("{:02}-{:02} {:02}:{:02}", a.tm_mon + 1, a.tm_mday, a.tm_hour, a.tm_min)
    }
}

/// `2026-11-23 14:02:07`
pub fn stamp(ms: i64) -> String {
    let tm = local(ms);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Parses a time bound (`7d`, `24h`, `30m`, `60s`, `2w`, `1h30m`, `all`,
/// `YYYY-MM-DD`, `YYYY-MM-DD HH:MM[:SS]`, or Unix timestamp). `Ok(None)` means
/// unbounded (`all`). When `end_of_day` is true, a bare `YYYY-MM-DD` resolves
/// to `23:59:59.999` local time instead of `00:00:00.000`.
pub fn parse_bound(spec: &str, now: i64, end_of_day: bool) -> Result<Option<i64>, String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("empty time specification".into());
    }
    if s.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    if let Some(dur) = parse_rel_duration_ms(s) {
        return Ok(Some(now - dur));
    }
    if s.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = s.parse::<i64>()
    {
        if n >= 1_000_000_000_000 {
            return Ok(Some(n));
        }
        if n >= 1_000_000_000 {
            return Ok(Some(n * 1000));
        }
    }
    if let Some(ms) = parse_local_datetime(s, end_of_day) {
        return Ok(Some(ms));
    }
    Err(format!(
        "cannot parse time {spec:?} (expected e.g. 7d, 24h, 30m, 2026-11-20, 2026-11-20 14:00, or all)"
    ))
}

fn parse_rel_duration_ms(s: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut num: Option<i64> = None;
    for ch in s.chars() {
        if let Some(d) = ch.to_digit(10) {
            num = Some(num.unwrap_or(0).checked_mul(10)?.checked_add(d as i64)?);
        } else {
            let n = num.take()?;
            let unit = match ch {
                's' | 'S' => 1_000i64,
                'm' | 'M' => 60_000,
                'h' | 'H' => 3_600_000,
                'd' | 'D' => 86_400_000,
                'w' | 'W' => 7 * 86_400_000,
                _ => return None,
            };
            total = total.checked_add(n.checked_mul(unit)?)?;
        }
    }
    if num.is_some() {
        return None;
    }
    Some(total)
}

fn parse_local_datetime(s: &str, end_of_day: bool) -> Option<i64> {
    let (date_part, time_part) = match s.split_once([' ', 'T', 't']) {
        Some((d, t)) => (d.trim(), Some(t.trim())),
        None => (s, None),
    };
    let mut dp = date_part.split('-');
    let year: i32 = dp.next()?.parse().ok()?;
    let mon: i32 = dp.next()?.parse().ok()?;
    let mday: i32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1970..=9999).contains(&year) || !(1..=12).contains(&mon) || !(1..=31).contains(&mday) {
        return None;
    }
    let (hour, min, sec, extra_ms) = match time_part {
        None if end_of_day => (23, 59, 59, 999),
        None => (0, 0, 0, 0),
        Some(t) => {
            let mut tp = t.split(':');
            let h: i32 = tp.next()?.parse().ok()?;
            let m: i32 = tp.next()?.parse().ok()?;
            let (sec, ms) = match tp.next() {
                Some(s_str) => (s_str.parse::<i32>().ok()?, if end_of_day { 999 } else { 0 }),
                None => {
                    if end_of_day {
                        (59, 999)
                    } else {
                        (0, 0)
                    }
                }
            };
            if tp.next().is_some() || !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=60).contains(&sec) {
                return None;
            }
            (h, m, sec, ms)
        }
    };
    // SAFETY: zeroed libc::tm passed to mktime with tm_isdst = -1.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = mon - 1;
        tm.tm_mday = mday;
        tm.tm_hour = hour;
        tm.tm_min = min;
        tm.tm_sec = sec;
        tm.tm_isdst = -1;
        let t = libc::mktime(&mut tm);
        if t == -1 {
            return None;
        }
        t as i64
    };
    Some(secs * 1000 + extra_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bounds() {
        let now = 1_700_000_000_000i64;
        assert_eq!(parse_bound("all", now, false).unwrap(), None);
        assert_eq!(parse_bound("7d", now, false).unwrap(), Some(now - 7 * 86_400_000));
        assert_eq!(parse_bound("1h30m", now, false).unwrap(), Some(now - 5_400_000));
        assert_eq!(parse_bound("2w", now, false).unwrap(), Some(now - 14 * 86_400_000));
        let start = parse_bound("2026-11-23", now, false).unwrap().unwrap();
        let end = parse_bound("2026-11-23", now, true).unwrap().unwrap();
        assert_eq!(end - start, 86_400_000 - 1);
        let hm = parse_bound("2026-11-23 14:02:07", now, false).unwrap().unwrap();
        assert_eq!(stamp(hm), "2026-11-23 14:02:07");
        assert!(parse_bound("bogus", now, false).is_err());
    }
}
