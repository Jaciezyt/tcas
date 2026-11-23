//! Briefs: one line written by a session for other sessions (leases.md section 5).

/// Keeps the first non-empty line, drops control characters, collapses runs of
/// whitespace and cuts to `max_chars`. Returns `None` when nothing is left.
pub fn sanitize(raw: &str, max_chars: usize) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut out = String::new();
    let mut last_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else if c.is_control() {
            continue;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    let out: String = out.trim().chars().take(max_chars).collect();
    if out.is_empty() { None } else { Some(out) }
}

/// How a brief is shown to other sessions: quoted, so it reads as someone
/// else's words rather than an instruction.
pub fn quoted(brief: &str) -> String {
    format!("\"{}\"", brief.replace('"', "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_only_and_capped() {
        let long = "x".repeat(300);
        assert_eq!(sanitize(&format!("{long}\nsecond"), 200).unwrap().len(), 200);
        assert_eq!(sanitize("\n  fix\tthe   bug \nmore", 200).as_deref(), Some("fix the bug"));
        assert_eq!(sanitize("a\u{7}b", 200).as_deref(), Some("ab"));
        assert_eq!(sanitize("   \n ", 200), None);
    }

    #[test]
    fn counts_characters_not_bytes() {
        assert_eq!(sanitize("改名改名改名", 4).as_deref(), Some("改名改名"));
    }
}
