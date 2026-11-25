//! Containment tests on absolute, already normalized paths.

/// True if `inner` is `outer` or lies below it.
pub fn contains(outer: &str, inner: &str, case_insensitive: bool) -> bool {
    let (o, i) = if case_insensitive {
        (outer.to_lowercase(), inner.to_lowercase())
    } else {
        (outer.to_string(), inner.to_string())
    };
    let o = o.trim_end_matches('/');
    if o.is_empty() {
        return i.starts_with('/');
    }
    i == o || (i.len() > o.len() && i.starts_with(o) && i.as_bytes()[o.len()] == b'/')
}

/// `inner` relative to `outer` ("" when equal), keeping `inner`'s spelling.
/// `outer` must contain `inner`.
pub fn relative(outer: &str, inner: &str) -> String {
    // By components, not bytes: `outer` may contain `inner` only with the
    // case folded, which can change a path's length.
    let n = outer.trim_end_matches('/').split('/').count();
    inner.splitn(n + 1, '/').nth(n).unwrap_or("").trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment() {
        assert!(contains("/a/b", "/a/b", false));
        assert!(contains("/a/b", "/a/b/c", false));
        assert!(!contains("/a/b", "/a/bc", false));
        assert!(!contains("/a/B", "/a/b/c", false));
        assert!(contains("/a/B", "/a/b/c", true));
        assert!(contains("/", "/x", false));
    }

    #[test]
    fn relative_part() {
        assert_eq!(relative("/a/b", "/a/b/c/d.ts"), "c/d.ts");
        assert_eq!(relative("/a/b", "/a/b"), "");
        assert_eq!(relative("/a/B", "/a/b/X"), "X");
    }

    #[test]
    fn relative_goes_by_components() {
        assert_eq!(relative("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative("/a/b/", "/a/b"), "");
        assert_eq!(relative("/", "/x/y"), "x/y");
        assert!(contains("/W/İ", "/w/İ/x", true));
        assert_eq!(relative("/W/İ", "/w/İ/x"), "x");
    }
}
