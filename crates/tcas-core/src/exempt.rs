//! Exempt directories (leases.md section 3). Matching only; building the list
//! needs the environment and lives in tcas-sys.

use serde::{Deserialize, Serialize};

use crate::abspath;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptDir {
    /// Normalized absolute path that is matched.
    pub path: String,
    /// How the entry was written, for messages (`/tmp`, `~/scratch`).
    pub written: String,
    /// `builtin: system temp directory`, `config: ~/.tcas/config.toml`, ...
    pub source: String,
    pub case_insensitive: bool,
}

/// The exempt directory containing `abs`, if any.
pub fn find<'a>(list: &'a [ExemptDir], abs: &str) -> Option<&'a ExemptDir> {
    list.iter().find(|e| abspath::contains(&e.path, abs, e.case_insensitive))
}

/// The first exempt directory lying inside `dir` (or equal to it).
pub fn inside<'a>(list: &'a [ExemptDir], dir: &str, case_insensitive: bool) -> Option<&'a ExemptDir> {
    list.iter().find(|e| abspath::contains(dir, &e.path, case_insensitive || e.case_insensitive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(p: &str) -> ExemptDir {
        ExemptDir { path: p.into(), written: p.into(), source: "test".into(), case_insensitive: false }
    }

    #[test]
    fn finds_containing_entry() {
        let list = vec![e("/private/tmp"), e("/tmp")];
        assert_eq!(find(&list, "/private/tmp/x/y").unwrap().path, "/private/tmp");
        assert_eq!(find(&list, "/tmp").unwrap().path, "/tmp");
        assert!(find(&list, "/tmpx/a").is_none());
        assert_eq!(inside(&list, "/private", false).unwrap().path, "/private/tmp");
    }
}
