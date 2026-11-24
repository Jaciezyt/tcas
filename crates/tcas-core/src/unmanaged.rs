//! Unmanaged path patterns from `.tcas.toml` (`unmanaged`, leases.md section 3,
//! daemon-and-cli.md section 6).

use serde::{Deserialize, Serialize};

use crate::relpath;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmanagedRule {
    /// Directory containing the `.tcas.toml`, relative to the workspace root
    /// (`""` for the root itself).
    pub base_rel: String,
    pub pattern: String,
    pub source: String,
    pub ci: bool,
}

/// Returns the first rule matching `rel` (a root-relative file path).
pub fn find<'a>(rules: &'a [UnmanagedRule], rel: &str) -> Option<&'a UnmanagedRule> {
    rules.iter().find(|r| matches_rule(r, rel))
}

pub fn matches_rule(rule: &UnmanagedRule, rel: &str) -> bool {
    let bk = relpath::key(&rule.base_rel, rule.ci);
    let rk = relpath::key(rel, rule.ci);
    if !relpath::contains(&bk, &rk) {
        return false;
    }
    let sub = relpath::strip(&rule.base_rel, rel);
    if sub.is_empty() {
        return false;
    }
    let raw = rule.pattern.as_str();
    if raw.is_empty() || raw.ends_with('/') {
        return false;
    }
    let anchored = raw.starts_with('/');
    let pat = raw.strip_prefix('/').unwrap_or(raw);
    if pat.is_empty() {
        return false;
    }
    let (pat_s, sub_s) = if rule.ci {
        (pat.to_lowercase(), sub.to_lowercase())
    } else {
        (pat.to_string(), sub)
    };
    if !anchored && !pat_s.contains('/') {
        let base = sub_s.rsplit('/').next().unwrap_or(&sub_s);
        glob_match(&pat_s, base)
    } else {
        glob_match(&pat_s, &sub_s)
    }
}

fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let w = t.len() + 1;
    let mut memo = vec![None; (p.len() + 1) * w];
    match_at(&p, &t, 0, 0, w, &mut memo)
}

fn match_at(p: &[char], t: &[char], pi: usize, ti: usize, w: usize, memo: &mut [Option<bool>]) -> bool {
    let idx = pi * w + ti;
    if let Some(res) = memo[idx] {
        return res;
    }
    let res = if pi == p.len() {
        ti == t.len()
    } else if pi + 2 < p.len() && p[pi] == '*' && p[pi + 1] == '*' && p[pi + 2] == '/' {
        match_at(p, t, pi + 3, ti, w, memo)
            || t[ti..]
                .iter()
                .position(|&c| c == '/')
                .is_some_and(|off| match_at(p, t, pi, ti + off + 1, w, memo))
    } else if pi + 1 < p.len() && p[pi] == '*' && p[pi + 1] == '*' {
        match_at(p, t, pi + 2, ti, w, memo) || (ti < t.len() && match_at(p, t, pi, ti + 1, w, memo))
    } else if p[pi] == '*' {
        match_at(p, t, pi + 1, ti, w, memo) || (ti < t.len() && t[ti] != '/' && match_at(p, t, pi, ti + 1, w, memo))
    } else if p[pi] == '?' {
        ti < t.len() && t[ti] != '/' && match_at(p, t, pi + 1, ti + 1, w, memo)
    } else {
        ti < t.len() && p[pi] == t[ti] && match_at(p, t, pi + 1, ti + 1, w, memo)
    };
    memo[idx] = Some(res);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(base_rel: &str, pattern: &str, ci: bool) -> UnmanagedRule {
        UnmanagedRule {
            base_rel: base_rel.into(),
            pattern: pattern.into(),
            source: ".tcas.toml".into(),
            ci,
        }
    }

    #[test]
    fn basename_pattern_matches_any_depth() {
        let r = rule("", "pnpm-lock.yaml", false);
        assert!(matches_rule(&r, "pnpm-lock.yaml"));
        assert!(matches_rule(&r, "packages/web/pnpm-lock.yaml"));
        assert!(!matches_rule(&r, "pnpm-lock.yaml.bak"));
        assert!(!matches_rule(&r, "my-pnpm-lock.yaml"));

        let glob = rule("", "*.lock", false);
        assert!(matches_rule(&glob, "Cargo.lock"));
        assert!(matches_rule(&glob, "a/b/yarn.lock"));
        assert!(!matches_rule(&glob, "lock"));
    }

    #[test]
    fn leading_slash_anchors_to_base() {
        let r = rule("", "/pnpm-lock.yaml", false);
        assert!(matches_rule(&r, "pnpm-lock.yaml"));
        assert!(!matches_rule(&r, "sub/pnpm-lock.yaml"));

        let nested = rule("web", "/pnpm-lock.yaml", false);
        assert!(matches_rule(&nested, "web/pnpm-lock.yaml"));
        assert!(!matches_rule(&nested, "web/sub/pnpm-lock.yaml"));
        assert!(!matches_rule(&nested, "pnpm-lock.yaml"));
    }

    #[test]
    fn slash_patterns_and_double_star() {
        let snap = rule("", "**/*.snap", false);
        assert!(matches_rule(&snap, "a.snap"));
        assert!(matches_rule(&snap, "src/a.snap"));
        assert!(matches_rule(&snap, "src/ui/btn.snap"));
        assert!(!matches_rule(&snap, "src/a.snap.bak"));

        let generated = rule("", "generated/*.ts", false);
        assert!(matches_rule(&generated, "generated/types.ts"));
        assert!(!matches_rule(&generated, "generated/sub/types.ts"));
        assert!(!matches_rule(&generated, "src/generated/types.ts"));

        let mid = rule("api", "src/**/index.ts", false);
        assert!(matches_rule(&mid, "api/src/index.ts"));
        assert!(matches_rule(&mid, "api/src/v1/index.ts"));
        assert!(matches_rule(&mid, "api/src/v1/auth/index.ts"));
        assert!(!matches_rule(&mid, "web/src/index.ts"));

        let dist = rule("", "dist/**", false);
        assert!(matches_rule(&dist, "dist/a.js"));
        assert!(matches_rule(&dist, "dist/a/b.js"));
    }

    #[test]
    fn single_char_and_case_insensitivity() {
        let q = rule("", "file?.txt", false);
        assert!(matches_rule(&q, "file1.txt"));
        assert!(!matches_rule(&q, "file12.txt"));
        assert!(!matches_rule(&q, "file/.txt"));

        let ci = rule("Web", "*.LOCK", true);
        assert!(matches_rule(&ci, "web/Cargo.lock"));
        assert!(!matches_rule(&rule("Web", "*.LOCK", false), "web/Cargo.lock"));
    }
}
