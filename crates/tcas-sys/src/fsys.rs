//! Path normalization (leases.md section 3, steps 1 and 2) and volume probes.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Joins `p` onto `cwd` when relative and removes `.` and `..` lexically.
pub fn absolute(cwd: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    out
}

/// Resolves symlinks in the parent directories, not in the last component.
/// When parents do not exist yet, resolves the nearest one that does and
/// appends the rest. `p` must be absolute and lexically clean.
pub fn normalize(p: &Path) -> PathBuf {
    let Some(name) = p.file_name() else {
        return p.to_path_buf();
    };
    let parent = p.parent().unwrap_or(Path::new("/"));
    let mut base = parent.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&base) {
            let mut out = real;
            for t in tail.iter().rev() {
                out.push(t);
            }
            out.push(name);
            return out;
        }
        match (base.file_name().map(|n| n.to_os_string()), base.parent()) {
            (Some(n), Some(up)) => {
                tail.push(n);
                base = up.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Resolves every component, for directories that must exist (workspace roots).
pub fn canonical_dir(p: &Path) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(p)?;
    if !real.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"));
    }
    Ok(real)
}

/// Whether the volume holding `p` compares names without regard to case.
#[cfg(target_os = "macos")]
pub fn case_insensitive(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid NUL-terminated path; pathconf only reads it.
    let r = unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) };
    r == 0
}

#[cfg(not(target_os = "macos"))]
pub fn case_insensitive(p: &Path) -> bool {
    let _ = (p, CString::new("").ok(), OsStrExt::as_bytes(p.as_os_str()));
    false
}

pub fn to_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical() {
        assert_eq!(absolute(Path::new("/a/b"), Path::new("../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(absolute(Path::new("/a"), Path::new("/x/../y")), PathBuf::from("/y"));
    }

    #[test]
    fn resolves_parents_only() {
        let tmp = std::env::temp_dir();
        let real_tmp = std::fs::canonicalize(&tmp).unwrap();
        let p = normalize(&tmp.join("tcas-no-such-dir/sub/file.txt"));
        assert_eq!(p, real_tmp.join("tcas-no-such-dir/sub/file.txt"));
    }
}
