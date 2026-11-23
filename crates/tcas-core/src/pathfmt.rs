//! Formatting absolute paths for people and for commands they can paste.

use std::path::{Component, Path, PathBuf};

/// `to` written relative to the directory `from`, lexically. Both must be absolute.
/// Returns "." when they are the same.
pub fn relative(from: &Path, to: &Path) -> PathBuf {
    let f: Vec<Component> = from.components().collect();
    let t: Vec<Component> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// A path the viewer in `cwd` can use: relative when that is short, absolute
/// (with `~` for the home directory) when the relative form climbs too far.
pub fn for_viewer(cwd: &Path, abs: &Path, home: Option<&Path>, is_dir: bool) -> String {
    let rel = relative(cwd, abs);
    let ups = rel.components().take_while(|c| matches!(c, Component::ParentDir)).count();
    let mut s = if ups <= 2 {
        rel.to_string_lossy().into_owned()
    } else {
        tilde(abs, home)
    };
    if is_dir && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// An absolute path with the home directory written as `~`.
pub fn tilde(abs: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home
        && let Ok(rest) = abs.strip_prefix(h)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.to_string_lossy())
        };
    }
    abs.to_string_lossy().into_owned()
}

/// Quotes a word for a POSIX shell when it needs it.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./~:@%+=,".contains(c));
    if plain && !s.starts_with('~') {
        s.to_string()
    } else if plain {
        // Keep a leading ~ unquoted so the shell expands it.
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d")), PathBuf::from("c/d"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x/y")), PathBuf::from("../x/y"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), PathBuf::from("."));
    }

    #[test]
    fn viewer_paths() {
        let home = Path::new("/Users/u");
        assert_eq!(
            for_viewer(Path::new("/Users/u/Code/app"), Path::new("/Users/u/Code/app-backend/src/a.ts"), Some(home), false),
            "../app-backend/src/a.ts"
        );
        assert_eq!(
            for_viewer(Path::new("/Users/u/Code/app/web/src/x"), Path::new("/Users/u/Code/other/a"), Some(home), true),
            "~/Code/other/a/"
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("src/a.ts"), "src/a.ts");
        assert_eq!(shell_quote("my file"), "'my file'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
