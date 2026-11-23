//! Texts for the model: denial reasons and notices (harness.md sections 4
//! and 6). Harness-neutral: how to run a command in the background is added
//! by the adapter in `tcas hook`.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tcas_core::brief;
use tcas_core::human::{self, short_session};
use tcas_core::pathfmt::{self, shell_quote};
use tcas_core::proto::{AcquireResult, BlockerView, Outcome, PathRef};
use tcas_sys::clock;

/// Notices injected at once are capped at this many characters (harness.md
/// section 6); the rest wait for the next hook.
pub const MAX_CONTEXT: usize = 10_000;

/// Who reads the text: paths in commands are written from their directory.
pub struct Viewer {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub now: i64,
}

impl Viewer {
    pub fn new(cwd: &Path, now: i64) -> Viewer {
        let home = tcas_sys::home::user_home().map(|h| std::fs::canonicalize(&h).unwrap_or(h));
        Viewer { cwd: cwd.to_path_buf(), home, now }
    }

    pub fn arg(&self, p: &PathRef) -> String {
        shell_quote(&self.path(&p.abs, p.is_dir))
    }

    /// An absolute path written from the viewer's directory, unquoted.
    pub fn path(&self, abs: &str, is_dir: bool) -> String {
        pathfmt::for_viewer(&self.cwd, Path::new(abs), self.home.as_deref(), is_dir)
    }

    pub fn tilde(&self, abs: &str) -> String {
        pathfmt::tilde(Path::new(abs), self.home.as_deref())
    }
}

/// A denial for the hook response.
pub struct Denial {
    pub reason: String,
    pub wait_command: Option<String>,
    pub wait_many: bool,
    pub needs_user: bool,
}

fn brief_line(b: &BlockerView, ended: bool) -> String {
    let l = &b.lease;
    match &l.brief {
        Some(t) if l.source == "grant" && !l.brief_is_session => {
            format!("  Their brief (written by the user): {}", brief::quoted(t))
        }
        Some(t) if ended => format!("  Their brief was: {}", brief::quoted(t)),
        Some(t) => format!("  Their brief: {}", brief::quoted(t)),
        None if ended => "  They had not written a brief.".into(),
        None => "  They have not written a brief.".into(),
    }
}

/// `x is` or `x is in d/, which is`: a file blocked by a directory lease
/// names the directory.
fn subject(b: &BlockerView) -> String {
    let (want, held) = (&b.want.display, &b.lease.path.display);
    if want == held || b.want.is_dir && held.starts_with(want.as_str()) {
        format!("{held} is")
    } else {
        format!("{want} is in {held}, which is")
    }
}

/// The reason an edit was refused (harness.md section 4). `ended_at` gives
/// when a holder session ended, for orphaned leases.
pub fn denial(v: &Viewer, r: &AcquireResult, ended_at: impl Fn(&str) -> Option<i64>) -> Denial {
    let mut lines: Vec<String> = Vec::new();
    let mut needs_user = false;
    let mut any_brief = false;
    let mut all_orphaned = true;
    for b in &r.blockers {
        let l = &b.lease;
        let who = short_session(&l.session);
        if b.reason == "orphaned" || l.holder_state == "ended" {
            let dirty = matches!(b.reason.as_str(), "orphaned" | "uncommitted");
            needs_user = needs_user || dirty;
            let when = ended_at(&l.session).map(|t| format!(", ended {}", clock::short(t, v.now))).unwrap_or_default();
            lines.push(format!("tcas: {} held by a session that has ended ({who}{when}).", subject(b)));
            lines.push(brief_line(b, true));
            if dirty {
                lines.push(
                    "The file still has uncommitted changes. It stays leased until the user commits or discards those \
                     changes, transfers the lease, or force-releases it. Tell the user."
                        .into(),
                );
            }
            continue;
        }
        all_orphaned = false;
        any_brief = any_brief || l.brief.is_some();
        lines.push(format!(
            "tcas: {} leased by another session ({who}, since {}).",
            subject(b),
            clock::short(l.acquired_at, v.now)
        ));
        lines.push(brief_line(b, false));
        let means = "Do not modify it by any other means, including Bash.";
        if l.stalled {
            needs_user = true;
            lines.push(format!(
                "It has uncommitted changes, and that session has had no activity for {}. It stays leased until the \
                 user handles it (that session goes on, or the user commits, transfers or force-releases it). {means} \
                 Tell the user.",
                human::duration(l.holder_idle_ms)
            ));
            continue;
        }
        lines.push(match b.reason.as_str() {
            "uncommitted" => format!("It has uncommitted work in progress. {means}"),
            "busy" => format!("They are changing it right now. {means}"),
            "grace" => format!("It was just given to them after a wait. {means}"),
            _ if l.path.is_dir => "They hold the whole directory. Do not modify files in it by any other means, including Bash.".into(),
            _ => format!("They requested it in advance. {means}"),
        });
    }
    if any_brief && !all_orphaned {
        lines.push("If their brief already covers the change you were about to make, do not wait: tell the user.".into());
    }
    let wait = if r.wanted.is_empty() {
        None
    } else {
        let paths: Vec<String> = r.wanted.iter().map(|p| v.arg(p)).collect();
        let what = if r.wanted.len() == 1 { "it" } else { "them" };
        Some(format!("tcas lease wait {} --brief \"<one line: what you need {what} for>\"", paths.join(" ")))
    };
    Denial { reason: lines.join("\n"), wait_command: wait, wait_many: r.wanted.len() > 1, needs_user }
}

pub fn strict(v: &Viewer, wanted: &[PathRef]) -> String {
    let shown: Vec<&str> = wanted.iter().map(|p| p.display.as_str()).collect();
    let args: Vec<String> = wanted.iter().map(|p| v.arg(p)).collect();
    format!(
        "tcas: this workspace is in strict mode, and this session does not hold {}. Request it before editing:\n  \
         tcas lease acquire {} --brief \"<one line: what you are changing and why>\"\nFor a larger change, request \
         the whole area at once (a directory, or several files).",
        shown.join(", "),
        args.join(" ")
    )
}

pub fn config_file(shown: &str, what: &str) -> String {
    format!("tcas: {shown} is {what}. Only the user can change it; tell the user what you need.")
}

pub fn outside_hint(shown: &str) -> String {
    format!(
        "tcas: you edited a file in {shown}, which is outside this workspace. Edits there are\nnot coordinated with \
         other sessions. If you will keep working there, add it to the workspace:\n  tcas workspace add {}",
        shell_quote(shown)
    )
}

pub fn took_over(from: &[String], paths: &[String]) -> String {
    let who: Vec<String> = from.iter().map(|s| short_session(s)).collect();
    format!(
        "tcas: this session continues the one before it in this process ({}) and now holds its leases: {}.\nSome \
         of these files may have uncommitted changes made before; look at them before editing.",
        who.join(", "),
        paths.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Notices

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s}  ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

/// All `lease.yielded` notices as one block (harness.md section 6).
pub fn yielded(bodies: &[Value]) -> Option<String> {
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut all_committed = true;
    let mut kept: Vec<String> = Vec::new();
    for b in bodies {
        let paths = strs(b, "paths");
        let whys = strs(b, "why");
        let to = short_session(s(b, "to"));
        let to = match b.get("to_brief").and_then(Value::as_str) {
            Some(t) => format!("{to} {}", brief::quoted(t)),
            None => to,
        };
        for (i, p) in paths.iter().enumerate() {
            let why = whys.get(i).cloned().unwrap_or_default();
            if why != "committed_idle" {
                all_committed = false;
            }
            rows.push((p.clone(), to.clone(), why));
        }
        kept = strs(b, "still_held");
    }
    if rows.is_empty() {
        return None;
    }
    let w = rows.iter().map(|r| r.0.chars().count() + 4).max().unwrap_or(0);
    let mut out = Vec::new();
    if all_committed {
        out.push("tcas: while this session was idle, these leases were given to other sessions:".to_string());
        for (p, to, _) in &rows {
            out.push(format!("  {}to {to}", pad(p, w)));
        }
        out.push(
            "You had committed them; nothing of yours is lost. Before changing them again, request them\nagain \
             (tcas lease acquire …) and re-read them: they may have changed since."
                .into(),
        );
    } else {
        out.push("tcas: these leases of yours were given to other sessions:".to_string());
        for (p, to, why) in &rows {
            let note = match why.as_str() {
                "committed_idle" => " (committed, your turn had ended)",
                "expired" => " (not used for a long time)",
                _ => " (no uncommitted changes)",
            };
            out.push(format!("  {}to {to}{note}", pad(p, w)));
        }
        out.push(
            "None had uncommitted changes, so nothing of yours is lost. Before changing them again, request\nthem \
             again (tcas lease acquire …) and re-read them: they may have changed since."
                .into(),
        );
    }
    if !kept.is_empty() {
        out.push(format!("You still hold: {}", kept.join(", ")));
    }
    Some(out.join("\n"))
}

fn displays(ps: &[PathRef]) -> String {
    ps.iter().map(|p| p.display.as_str()).collect::<Vec<_>>().join(", ")
}

fn wait_result(r: &AcquireResult) -> String {
    let waited = r.waited_ms.map(human::duration).unwrap_or_else(|| "0s".into());
    let wanted = displays(&r.wanted);
    match r.outcome {
        Outcome::Granted => {
            let got: Vec<PathRef> = r.granted.iter().map(|g| g.path.clone()).collect();
            let mut out = vec![format!(
                "tcas: your wait is over (after {waited}). This session now holds: {}. Re-read {} before editing.",
                displays(&got),
                if got.len() == 1 { "it" } else { "them" }
            )];
            for g in &r.granted {
                if let Some(c) = &g.changed_in {
                    out.push(format!("  {} changed while you waited (commit {c}).", g.path.display));
                }
                if g.foreign_changes {
                    out.push(format!("  {} has uncommitted changes that you did not make.", g.path.display));
                }
            }
            out.join("\n")
        }
        Outcome::Timeout => {
            let mut out = vec![format!("tcas: your wait for {wanted} timed out after {waited}; nothing was acquired.")];
            for b in &r.blockers {
                let t = b.lease.brief.as_deref().map(|t| format!(" {}", brief::quoted(t))).unwrap_or_default();
                out.push(format!("  {}  still leased by {}{t}", b.lease.path.display, short_session(&b.lease.session)));
            }
            out.push("Wait again with the same command, or tell the user.".into());
            out.join("\n")
        }
        Outcome::Deadlock => {
            let mut out = vec![format!("tcas: your wait for {wanted} stopped after {waited}: it would deadlock.")];
            let mut mine = Vec::new();
            for st in &r.cycle {
                let whose = if st.next.is_empty() {
                    mine.push(st.waits_for.display.clone());
                    "you hold".to_string()
                } else {
                    format!("{} holds", short_session(&st.next))
                };
                out.push(format!(
                    "  {} holds {} and is waiting for {}, which {whose}.",
                    short_session(&st.session),
                    st.holds.display,
                    st.waits_for.display
                ));
            }
            if mine.is_empty() {
                out.push("Nothing was acquired. Tell the user.".into());
            } else {
                out.push(format!("Nothing was acquired. Commit or release {} first, or tell the user.", mine.join(", ")));
            }
            out.join("\n")
        }
        _ => format!("tcas: your wait for {wanted} ended: {}.", r.refusal.as_deref().unwrap_or("cancelled")),
    }
}

/// One notice other than `lease.yielded`.
pub fn notice(kind: &str, b: &Value) -> Option<String> {
    Some(match kind {
        "wait.granted" | "wait.timeout" | "wait.deadlock" => {
            let r: AcquireResult = serde_json::from_value(b.get("result")?.clone()).ok()?;
            wait_result(&r)
        }
        "lease.revoked" => format!(
            "tcas: the user released your lease on {}. Another session may now change it. Before changing it again, \
             request it again and re-read it.",
            s(b, "path")
        ),
        "lease.granted_by_user" => {
            let mut t = format!("tcas: the user gave this session leases on: {}.", strs(b, "paths").join(", "));
            if let Some(br) = b.get("brief").and_then(Value::as_str) {
                t.push_str(&format!("\n  The user's brief: {}", brief::quoted(br)));
            }
            t
        }
        "lease.transferred_away" => format!(
            "tcas: the user moved your lease on {} to {}. Do not change it any more.",
            s(b, "path"),
            short_session(s(b, "to"))
        ),
        "lease.transferred_to_you" => format!(
            "tcas: the user moved the lease on {} to this session (from {}). It may have uncommitted changes made \
             there; look at them before editing.",
            s(b, "path"),
            short_session(s(b, "from"))
        ),
        "edit.foreign_changes" => format!(
            "tcas: {} had uncommitted changes that this session did not make. Look at them before editing further.",
            strs(b, "paths").join(", ")
        ),
        _ => return None,
    })
}
