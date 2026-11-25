//! Paths relative to a workspace root: "/"-separated, no leading or trailing slash.
//! The root itself is "". Comparisons use keys (see [`key`]).

/// The comparison form of a root-relative path. On a case-insensitive volume the
/// case is folded, so `Src/App.ts` and `src/app.ts` are the same path.
pub fn key(path: &str, case_insensitive: bool) -> String {
    if case_insensitive {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

/// True if `inner` is `outer` or lies below it. Both are keys. The root ("")
/// contains everything.
pub fn contains(outer: &str, inner: &str) -> bool {
    if outer.is_empty() {
        return true;
    }
    inner == outer
        || (inner.len() > outer.len()
            && inner.starts_with(outer)
            && inner.as_bytes()[outer.len()] == b'/')
}

/// Whether two leased paths overlap (leases.md section 4).
pub fn intersects(a: &str, a_dir: bool, b: &str, b_dir: bool) -> bool {
    match (a_dir, b_dir) {
        (false, false) => a == b,
        (true, false) => contains(a, b),
        (false, true) => contains(b, a),
        (true, true) => contains(a, b) || contains(b, a),
    }
}

/// `a/b` joined with `c` gives `a/b/c`; either side may be empty.
pub fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// `inner` with `outer` taken off the front. `outer` must contain `inner`,
/// possibly only by key: it goes by components, not bytes, since folding the
/// case can change a path's length.
pub fn strip(outer: &str, inner: &str) -> String {
    if outer.is_empty() {
        return inner.to_string();
    }
    let n = outer.split('/').count();
    inner.splitn(n + 1, '/').nth(n).unwrap_or("").to_string()
}

/// The parent of a relative path; the parent of a top-level name is "".
pub fn parent(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[..i],
        None => "",
    }
}

/// The deepest directory containing all the given paths.
pub fn common_dir<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut it = paths.into_iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut common: Vec<&str> = parent(first).split('/').filter(|s| !s.is_empty()).collect();
    for p in it {
        let parts: Vec<&str> = parent(p).split('/').filter(|s| !s.is_empty()).collect();
        let n = common.iter().zip(&parts).take_while(|(a, b)| a == b).count();
        common.truncate(n);
    }
    common.join("/")
}

/// Display form: a directory gets a trailing slash.
pub fn display(path: &str, is_dir: bool) -> String {
    if is_dir && !path.is_empty() {
        format!("{path}/")
    } else if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment() {
        assert!(contains("src", "src"));
        assert!(contains("src", "src/a.ts"));
        assert!(!contains("src", "srcx/a.ts"));
        assert!(!contains("src/a", "src"));
        assert!(contains("", "anything"));
    }

    #[test]
    fn intersection_table() {
        assert!(intersects("a.txt", false, "a.txt", false));
        assert!(!intersects("a.txt", false, "b.txt", false));
        assert!(intersects("src", true, "src/x.ts", false));
        assert!(intersects("src/x.ts", false, "src", true));
        assert!(!intersects("src", true, "srcx.ts", false));
        assert!(intersects("src", true, "src/auth", true));
        assert!(intersects("src/auth", true, "src", true));
        assert!(!intersects("src/auth", true, "src/api", true));
    }

    #[test]
    fn case_folding() {
        assert_eq!(key("Src/App.ts", true), "src/app.ts");
        assert_eq!(key("Src/App.ts", false), "Src/App.ts");
    }

    #[test]
    fn helpers() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b/c"), "a/b/c");
        assert_eq!(strip("a", "a/b/c"), "b/c");
        assert_eq!(strip("", "a/b"), "a/b");
        assert_eq!(strip("a/b", "a/b"), "");
        assert_eq!(parent("a/b/c"), "a/b");
        assert_eq!(parent("a"), "");
        assert_eq!(common_dir(["src/auth/a.ts", "src/auth/b.ts", "src/auth/x/c.ts"]), "src/auth");
        assert_eq!(common_dir(["web/a.ts", "api/b.ts"]), "");
        assert_eq!(display("src", true), "src/");
        assert_eq!(display("", true), ".");
    }

    #[test]
    fn strip_goes_by_components() {
        assert_eq!(strip("a", "a/b/c"), "b/c");
        assert_eq!(strip("a/b", "a/b"), "");
        assert_eq!(strip("", "a"), "a");
        // Folding "İ" (2 bytes) gives "i̇" (3 bytes): the keys match, the
        // lengths do not.
        let outer = key("İ", true);
        assert!(contains(&outer, &key("İ/x.ts", true)));
        assert_eq!(strip(&outer, "İ/x.ts"), "x.ts");
    }
}
