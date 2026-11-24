//! Building the exempt directory list (leases.md section 3).

use std::path::{Path, PathBuf};

use tcas_core::config::GlobalConfig;
use tcas_core::exempt::ExemptDir;

use crate::fsys;
use crate::home::{Home, user_home};

fn expand(written: &str, home: Option<&Path>) -> Option<PathBuf> {
    if written == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = written.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    let p = PathBuf::from(written);
    if p.is_absolute() { Some(p) } else { None }
}

fn push(out: &mut Vec<ExemptDir>, path: &Path, written: &str, source: &str) {
    let clean = fsys::absolute(Path::new("/"), path);
    let mut forms = vec![clean.clone()];
    if let Ok(real) = std::fs::canonicalize(&clean) {
        forms.push(real);
    } else {
        forms.push(fsys::normalize(&clean));
    }
    for f in forms {
        let s = fsys::to_str(&f);
        if out.iter().any(|e| e.path == s) {
            continue;
        }
        let ci = fsys::case_insensitive(&f) || f.parent().is_some_and(fsys::case_insensitive);
        out.push(ExemptDir { path: s, written: written.into(), source: source.into(), case_insensitive: ci });
    }
}

/// The global list: built-in entries (unless turned off) and `[exempt] paths`.
/// An entry that cannot be read is reported in the second value.
pub fn global_list(cfg: &GlobalConfig, home: &Home) -> (Vec<ExemptDir>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let uhome = user_home();
    if cfg.exempt.builtin {
        let tmp_src = "built-in: system temp directory";
        push(&mut out, Path::new("/tmp"), "/tmp", tmp_src);
        push(&mut out, Path::new("/var/tmp"), "/var/tmp", tmp_src);
        if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
            let t = PathBuf::from(t);
            let written = fsys::to_str(&t).trim_end_matches('/').to_string();
            push(&mut out, &t, &written, tmp_src);
        }
        if cfg!(target_os = "linux") {
            push(&mut out, Path::new("/dev/shm"), "/dev/shm", tmp_src);
        }
        push(&mut out, &home.dir, &tilde(&home.dir, uhome.as_deref()), "built-in: tcas's own directory");
        if let Some(h) = &uhome {
            for d in [".claude", ".gemini", ".codex"] {
                push(&mut out, &h.join(d), &format!("~/{d}"), "built-in: harness directory");
            }
        }
    }
    for w in &cfg.exempt.paths {
        match expand(w, uhome.as_deref()) {
            Some(p) => push(&mut out, &p, w, "config: ~/.tcas/config.toml"),
            None => problems.push(format!("[exempt] paths: {w:?} is not absolute and does not start with ~")),
        }
    }
    (out, problems)
}

/// Entries from a workspace's `.tcas.toml`, relative to the directory holding it.
pub fn workspace_list(dir: &Path, entries: &[String], source: &str, case_insensitive: bool) -> Vec<ExemptDir> {
    let mut out = Vec::new();
    for w in entries {
        let p = fsys::absolute(dir, Path::new(w.trim_end_matches('/')));
        if !p.starts_with(dir) || p == dir {
            continue;
        }
        out.push(ExemptDir {
            path: fsys::to_str(&p),
            written: w.clone(),
            source: source.into(),
            case_insensitive,
        });
    }
    out
}

fn tilde(p: &Path, home: Option<&Path>) -> String {
    tcas_core::pathfmt::tilde(p, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_includes_resolved_tmp() {
        let home = Home { dir: PathBuf::from("/nonexistent/.tcas") };
        let (list, problems) = global_list(&GlobalConfig::default(), &home);
        assert!(problems.is_empty());
        let real = std::fs::canonicalize("/tmp").unwrap();
        let real = real.to_string_lossy();
        assert!(tcas_core::exempt::find(&list, &format!("{real}/x")).is_some());
        assert!(tcas_core::exempt::find(&list, "/tmp/x").is_some());
        if let Some(uh) = user_home() {
            for d in [".claude", ".gemini", ".codex"] {
                let p = format!("{}/{d}/brain/x", uh.to_string_lossy());
                assert!(tcas_core::exempt::find(&list, &p).is_some(), "missing built-in exempt for {d}");
            }
        }
        let mut cfg = GlobalConfig::default();
        cfg.exempt.builtin = false;
        assert!(global_list(&cfg, &home).0.is_empty());
    }
}
