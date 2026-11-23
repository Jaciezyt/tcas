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
