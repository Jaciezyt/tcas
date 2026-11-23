//! Short human forms of durations and counts.

/// `41s`, `3m`, `52m`, `1h`, `2h5m`, `3d`.
pub fn duration(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 { format!("{h}h") } else { format!("{h}h{m}m") }
    } else {
        format!("{}d", s / 86400)
    }
}

/// `1 file`, `3 files`.
pub fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// A session ID shortened for display: `claude:79583356-…` becomes `claude:7958…`.
pub fn short_session(id: &str) -> String {
    let (prefix, rest) = match id.split_once(':') {
        Some((p, r)) => (format!("{p}:"), r),
        None => (String::new(), id),
    };
    if rest.chars().count() <= 8 {
        id.to_string()
    } else {
        let head: String = rest.chars().take(4).collect();
        format!("{prefix}{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(41_000), "41s");
        assert_eq!(duration(3 * 60_000), "3m");
        assert_eq!(duration(3_600_000), "1h");
        assert_eq!(duration(3_600_000 + 5 * 60_000), "1h5m");
        assert_eq!(duration(3 * 86_400_000), "3d");
    }

    #[test]
    fn sessions() {
        assert_eq!(short_session("claude:79583356-aaaa"), "claude:7958…");
        assert_eq!(short_session("A"), "A");
        assert_eq!(short_session("manual:alpha"), "manual:alpha");
    }
}
