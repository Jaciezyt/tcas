//! Human-readable output of each command (daemon-and-cli.md section 5).

use std::path::{Path, PathBuf};

use tcas_core::brief;
use tcas_core::human::{self, short_session};
use tcas_core::pathfmt::{self, shell_quote};
use tcas_core::proto::*;
use tcas_sys::clock;
use tcas_sys::home::user_home;

pub struct Out {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub now: i64,
}

impl Out {
    pub fn new(cwd: &Path) -> Out {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let home = user_home().map(|h| std::fs::canonicalize(&h).unwrap_or(h));
        Out { cwd, home, now: clock::now_ms() }
    }

    pub fn tilde(&self, p: &str) -> String {
        pathfmt::tilde(Path::new(p), self.home.as_deref())
    }

    /// A path for a command the reader can paste (leases.md section 3).
    pub fn arg(&self, p: &PathRef) -> String {
        shell_quote(&pathfmt::for_viewer(&self.cwd, Path::new(&p.abs), self.home.as_deref(), p.is_dir))
    }

    fn since(&self, at: i64) -> String {
        clock::short(at, self.now)
    }

    fn ago(&self, at: i64) -> String {
        let d = self.now - at;
        if d < 60_000 { "now".into() } else { human::duration(d) }
    }
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s}  ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

fn width<'a>(items: impl Iterator<Item = &'a str>, min: usize) -> usize {
    items.map(|s| s.chars().count() + 2).max().unwrap_or(0).max(min)
}

fn join_displays(ps: &[PathRef]) -> String {
    ps.iter().map(|p| p.display.as_str()).collect::<Vec<_>>().join(", ")
}

pub fn unmanaged_reason(o: &Out, u: &Unmanaged) -> String {
    match &u.reason {
        UnmanagedReason::Exempt { written, source, .. } => format!("exempt directory {written} ({source})"),
        UnmanagedReason::GitExcluded { source, line, pattern } => {
            format!("excluded by git ({}:{line}: {pattern})", o.tilde(source))
        }
        UnmanagedReason::GitDir => "inside git's own directory (.git/)".into(),
        UnmanagedReason::OutsideWorkspace => "not in any workspace".into(),
    }
}

fn input_display(o: &Out, u: &Unmanaged) -> String {
    let p = if u.input.starts_with('/') { o.tilde(&u.abs) } else { u.input.clone() };
    if u.is_dir && !p.ends_with('/') { format!("{p}/") } else { p }
}

pub fn print_unmanaged(o: &Out, list: &[Unmanaged]) {
    if list.is_empty() {
        return;
    }
    println!("not managed, no lease needed:");
    let w = width(list.iter().map(|u| u.input.as_str()), 8) + 2;
    for u in list {
        println!("  {}{}", pad(&input_display(o, u), w), unmanaged_reason(o, u));
    }
}

fn brief_line(l: &LeaseView, indent: &str) -> String {
    match &l.brief {
        Some(b) if l.source == "grant" && !l.brief_is_session => {
            format!("{indent}their brief (written by the user): {}", brief::quoted(b))
        }
        Some(b) => format!("{indent}their brief: {}", brief::quoted(b)),
        None => format!("{indent}They have not written a brief."),
    }
}

fn holder_label(l: &LeaseView) -> String {
    let s = short_session(&l.session);
    match &l.holder_title {
        Some(t) => format!("{s} ({t})"),
        None => s,
    }
}

fn blocker_note(o: &Out, b: &BlockerView) -> String {
    let l = &b.lease;
    let mut note = match b.reason.as_str() {
        "uncommitted" => "uncommitted changes".to_string(),
        "orphaned" => "orphaned: its session ended with uncommitted changes".to_string(),
        "grace" => "just granted to it after a wait".to_string(),
        "busy" => "in use right now".to_string(),
        _ if l.path.is_dir => format!("directory, used {} ago", human::duration(o.now - l.touched_at)),
        _ => "requested in advance".to_string(),
    };
    if l.stalled {
        note.push_str(&format!("; no activity for {}", human::duration(l.holder_idle_ms)));
    }
    note
}

/// The path a blocker line starts with, and the "in <dir>, " note: a
/// directory request blocked by something inside it names that thing; a
/// request inside a leased directory names the directory.
fn blocker_head(b: &BlockerView) -> (&str, String) {
    let (want, held) = (&b.want.display, &b.lease.path.display);
    if want == held {
        (want, String::new())
    } else if b.want.is_dir && held.starts_with(want.as_str()) {
        (held, String::new())
    } else {
        (want, format!("in {held}, "))
    }
}

fn print_blockers(o: &Out, r: &AcquireResult, still: bool) {
    let w = width(r.blockers.iter().map(|b| blocker_head(b).0), 12) + 2;
    for b in &r.blockers {
        let l = &b.lease;
        let (head, within) = blocker_head(b);
        println!(
            "  {}{within}{}leased by {} since {} ({})",
            pad(head, w),
            if still { "still " } else { "" },
            holder_label(l),
            o.since(l.acquired_at),
            blocker_note(o, b)
        );
        println!("{}", brief_line(l, "      "));
    }
}

fn needs_user(r: &AcquireResult) -> bool {
    r.blockers.iter().any(|b| b.reason == "orphaned" || b.lease.stalled)
}

fn wait_command(o: &Out, r: &AcquireResult) -> String {
    let paths: Vec<String> = r.wanted.iter().map(|p| o.arg(p)).collect();
    let what = if r.wanted.len() == 1 { "it" } else { "them" };
    format!("tcas lease wait {} --brief \"<one line: what you need {what} for>\"", paths.join(" "))
}

pub fn acquire(o: &Out, r: &AcquireResult, grant_to: Option<&str>) -> i32 {
    match r.outcome {
        Outcome::Granted => {
            if !r.granted.is_empty() {
                let head = match grant_to {
                    Some(s) => format!("granted to {}", short_session(s)),
                    None => "granted".into(),
                };
                let items: Vec<String> = r
                    .granted
                    .iter()
                    .map(|g| {
                        let mut s = g.path.display.clone();
                        let mut notes = Vec::new();
                        if let Some(n) = g.files {
                            notes.push(format!("directory, {}", human::count(n, "tracked file", "tracked files")));
                        }
                        if g.renewed {
                            notes.push("already held, renewed".into());
                        }
                        if !notes.is_empty() {
                            s.push_str(&format!(" ({})", notes.join("; ")));
                        }
                        s
                    })
                    .collect();
                if items.len() == 1 {
                    println!("{head}: {}", items[0]);
                } else {
                    println!("{head}:");
                    for i in items {
                        println!("  {i}");
                    }
                }
                for g in r.granted.iter().filter(|g| g.foreign_changes) {
                    println!(
                        "note: {} has uncommitted changes that were not made by {}. Look at them before editing.",
                        g.path.display,
                        if grant_to.is_some() { "that session" } else { "you" }
                    );
                }
                if r.no_brief {
                    let paths: Vec<String> = r.granted.iter().map(|g| o.arg(&g.path)).collect();
                    println!("note: no brief. Other sessions cannot see what you are doing here. Add one with:");
                    println!(
                        "  tcas lease acquire {} --brief \"<one line: what you are changing and why>\"",
                        paths.join(" ")
                    );
                }
            }
            print_unmanaged(o, &r.unmanaged);
            0
        }
        Outcome::Refused => {
            if let Some(t) = &r.too_big {
                println!(
                    "refused: {} covers {} (limit {}). Request narrower directories, or list files.",
                    t.path.display,
                    human::count(t.files, "tracked file", "tracked files"),
                    t.limit
                );
                let parts: Vec<String> = t.children.iter().take(12).map(|(c, n)| format!("{c} {n}")).collect();
                println!("  {}{}", parts.join("   "), if t.children.len() > 12 { "   …" } else { "" });
            } else if let Some(m) = &r.refusal {
                println!("refused: {m}");
            }
            1
        }
        Outcome::Denied => {
            println!("denied: nothing was acquired.");
            print_blockers(o, r, false);
            if needs_user(r) {
                println!(
                    "Some of these stay leased until the user handles them (commit or discard the changes, or \
                     tcas lease transfer / tcas lease release --force). Tell the user."
                );
            }
            if r.blockers.iter().any(|b| b.lease.brief.is_some()) {
                println!("If their brief already covers the change you were about to make, do not wait: tell the user.");
            }
            let free = match r.wanted.len() {
                1 => format!("{} is", r.wanted[0].display),
                2 => "both are".into(),
                _ => "all of them are".into(),
            };
            println!("To wait until {free} free, run this as a background command:");
            println!("  {}", wait_command(o, r));
            if r.wanted.len() > 1 || r.wanted.iter().any(|p| p.is_dir) {
                println!("Or request only the parts you need.");
            }
            print_unmanaged(o, &r.unmanaged);
            3
        }
        _ => wait_result(o, r),
    }
}

pub fn wait_result(o: &Out, r: &AcquireResult) -> i32 {
    let waited = r.waited_ms.map(human::duration).unwrap_or_else(|| "0s".into());
    match r.outcome {
        Outcome::Granted => {
            let paths: Vec<PathRef> = r.granted.iter().map(|g| g.path.clone()).collect();
            if paths.is_empty() {
                print_unmanaged(o, &r.unmanaged);
                return 0;
            }
            println!("granted after {waited}: {}", join_displays(&paths));
            for g in &r.granted {
                if let Some(c) = &g.changed_in {
                    println!("  {} changed while you waited (commit {c}). Re-read it before editing.", g.path.display);
                }
                if g.foreign_changes {
                    println!("  {} has uncommitted changes that you did not make. Look at them before editing.", g.path.display);
                }
            }
            print_unmanaged(o, &r.unmanaged);
            0
        }
        Outcome::Timeout => {
            println!("timed out after {waited}: nothing was acquired.");
            print_blockers(o, r, true);
            if needs_user(r) {
                println!("Some of these stay leased until the user handles them. Tell the user.");
            } else {
                println!("Wait again with the same command, or tell the user.");
            }
            4
        }
        Outcome::Deadlock => {
            if r.waited_ms.is_some() {
                println!("deadlock: stopped waiting after {waited}; nothing was acquired.");
            } else {
                println!("deadlock: not waiting.");
            }
            let mut mine = Vec::new();
            for s in &r.cycle {
                let whose = if s.next.is_empty() {
                    mine.push(s.waits_for.display.clone());
                    "you hold".to_string()
                } else {
                    format!("{} holds", short_session(&s.next))
                };
                println!(
                    "  {} holds {} and is waiting for {}, which {whose}.",
                    short_session(&s.session),
                    s.holds.display,
                    s.waits_for.display
                );
            }
            if mine.is_empty() {
                println!("Release what others are waiting for first, or tell the user.");
            } else {
                println!("Commit or release {} first, or tell the user.", mine.join(", "));
            }
            5
        }
        Outcome::Cancelled => {
            println!("cancelled: {}", r.refusal.as_deref().unwrap_or("the wait was cancelled"));
            1
        }
        Outcome::Refused | Outcome::Denied => acquire(o, r, None),
    }
}

pub fn release(o: &Out, r: &ReleaseResult, forced: bool) -> i32 {
    if !r.released.is_empty() {
        println!("released: {}", join_displays(&r.released));
    }
    if !r.released_dirty.is_empty() {
        if forced {
            println!(
                "released (with uncommitted changes left in the files): {}",
                join_displays(&r.released_dirty)
            );
        } else {
            println!(
                "warning: released {} with uncommitted changes; another session may now change it.",
                join_displays(&r.released_dirty)
            );
        }
    }
    if !r.kept.is_empty() {
        println!("still held, uncommitted changes: {}", join_displays(&r.kept));
        let args: Vec<String> = r.kept.iter().map(|p| o.arg(p)).collect();
        println!("  Commit them, or to release them anyway name them:  tcas lease release {}", args.join(" "));
    }
    if forced && (!r.released.is_empty() || !r.released_dirty.is_empty()) {
        println!("The holders will be told.");
    }
    if !r.not_held.is_empty() {
        println!(
            "not held{}: {}",
            if forced { " by anyone" } else { " by you" },
            r.not_held.join(", ")
        );
    }
    print_unmanaged(o, &r.unmanaged);
    if r.released.is_empty() && r.released_dirty.is_empty() && r.kept.is_empty() && r.not_held.is_empty() && r.unmanaged.is_empty() {
        println!("nothing to release.");
    }
    0
}

fn lease_state(o: &Out, l: &LeaseView) -> String {
    let mut parts = Vec::new();
    if l.path.is_dir {
        let mut s = "dir".to_string();
        if let Some(n) = l.files {
            s.push_str(&format!(", {}", human::count(n, "file", "files")));
        }
        parts.push(s);
        let unused = o.now - l.touched_at;
        match l.yields.as_str() {
            "expired" => parts.push(format!("unused {}: yields on request", human::duration(unused))),
            "committed" => parts.push("committed, turn ended: yields on request".into()),
            _ => parts.push(format!("used {} ago", human::duration(unused))),
        }
    } else {
        match l.dirty {
            Some(true) => parts.push("modified".into()),
            Some(false) => parts.push("clean".into()),
            None => parts.push("not in a checkout".into()),
        }
        match l.yields.as_str() {
            "expired" => parts.push("unused, yields on request".into()),
            "committed" => parts.push("committed, yields on request".into()),
            _ => {}
        }
    }
    if l.state == "orphaned" {
        parts.push("ORPHANED".into());
    }
    if l.stalled {
        parts.push("STALLED".into());
    }
    parts.join(", ")
}

pub fn lease_list(o: &Out, ls: &[LeaseView], show_session: bool) -> i32 {
    if ls.is_empty() {
        println!("no leases.");
        return 0;
    }
    let w = width(ls.iter().map(|l| l.path.display.as_str()), 12);
    for l in ls {
        let who = if show_session { format!("{}  ", pad(&short_session(&l.session), 14)) } else { String::new() };
        println!("{}{who}{}{}  {}", pad(&l.path.display, w), pad(&l.source, 7), o.since(l.acquired_at), lease_state(o, l));
    }
    0
}

pub fn who(o: &Out, entries: &[WhoEntry]) -> i32 {
    let w = width(
        entries.iter().map(|e| e.path.as_ref().map(|p| p.display.as_str()).unwrap_or(e.input.as_str())),
        12,
    );
    for e in entries {
        if let Some(u) = &e.unmanaged {
            println!("{}not managed: {}", pad(&input_display(o, u), w), unmanaged_reason(o, u));
            continue;
        }
        let shown = e.path.as_ref().map(|p| p.display.clone()).unwrap_or_else(|| e.input.clone());
        if e.leases.is_empty() {
            println!("{}not leased", pad(&shown, w));
            continue;
        }
        for (i, l) in e.leases.iter().enumerate() {
            let label = if i == 0 { shown.clone() } else { String::new() };
            let within = if l.path.display != shown { format!("{} ", l.path.display) } else { String::new() };
            println!(
                "{}{within}leased by {} since {} ({}, {})",
                pad(&label, w),
                holder_label(l),
                o.since(l.acquired_at),
                l.source,
                lease_state(o, l)
            );
            println!("{}", brief_line(l, &" ".repeat(w + 2)));
        }
    }
    0
}

const BRANCH_CHANGED_RECENT_MS: i64 = 3_600_000;

fn branch_note(o: &Out, changed_at: Option<i64>) -> String {
    let Some(at) = changed_at else { return String::new() };
    let age = o.now - at;
    if !(0..=BRANCH_CHANGED_RECENT_MS).contains(&age) {
        return String::new();
    }
    format!("   (branch changed {} ago)", human::duration(age))
}

fn repo_branch(o: &Out, r: &RepoView) -> String {
    match &r.branch {
        Some(b) => format!("{b}{}", branch_note(o, r.branch_changed_at)),
        None => String::new(),
    }
}

fn repo_lines(o: &Out, ws: &WorkspaceView, root: Option<&str>) {
    let repos: Vec<&RepoView> = ws.repos.iter().filter(|r| root.is_none_or(|n| r.root == n)).collect();
    let w = width(repos.iter().map(|r| r.display.as_str()), 12);
    for r in repos {
        let shown = if ws.roots.len() > 1 && root.is_some() { o.tilde(&r.abs) } else { r.display.clone() };
        println!("  {}{}{}", pad(&shown, w.max(shown.len() + 2)), pad(&r.kind, 11), repo_branch(o, r));
    }
}

pub fn opened(o: &Out, r: &OpenResult, verb: &str) -> i32 {
    let ws = &r.workspace;
    let n = ws.repos.len();
    let checkouts = human::count(n, "checkout", "checkouts");
    if verb == "open" {
        if r.already_open {
            println!("already open: workspace \"{}\" (mode {}), {checkouts}:", ws.name, ws.mode);
        } else if ws.roots.len() == 1 {
            println!("opened {} (mode {}), {checkouts}{}", o.tilde(&ws.roots[0].path), ws.mode, if n > 0 { ":" } else { "." });
        } else {
            println!("opened workspace \"{}\" (mode {}) with {} roots, {checkouts}:", ws.name, ws.mode, ws.roots.len());
        }
        if ws.roots.len() == 1 {
            repo_lines(o, ws, None);
        } else {
            for root in &ws.roots {
                println!("  {}    {}", root.name, o.tilde(&root.path));
                let repos: Vec<&RepoView> = ws.repos.iter().filter(|x| x.root == root.name).collect();
                for x in repos {
                    println!("    {}{}{}", pad(&x.display, 28), pad(&x.kind, 11), x.branch.as_deref().unwrap_or(""));
                }
            }
        }
        if n == 0 {
            println!("note: no git checkouts found; leases here are not tied to git.");
        }
    } else if r.added.is_empty() {
        println!("nothing added: already part of workspace \"{}\".", ws.name);
    } else {
        println!("added to workspace \"{}\":", ws.name);
        for name in &r.added {
            let Some(root) = ws.roots.iter().find(|x| &x.name == name) else { continue };
            let repos: Vec<&RepoView> = ws.repos.iter().filter(|x| &x.root == name).collect();
            if repos.is_empty() {
                println!("  {}{}no checkouts", pad(name, 15), pad(&o.tilde(&root.path), 28));
            }
            for x in repos {
                println!(
                    "  {}{}{}{}",
                    pad(name, 15),
                    pad(&o.tilde(&x.abs), 28),
                    pad(&x.kind, 11),
                    x.branch.as_deref().unwrap_or("")
                );
            }
        }
        let names: Vec<String> = ws.roots.iter().map(|x| format!("{}/…", x.name)).collect();
        println!(
            "workspace \"{}\" now has {} roots. Paths are shown as {}",
            ws.name,
            ws.roots.len(),
            names.join(", ")
        );
    }
    if !r.outside_worktrees.is_empty() {
        println!("note: this repository has worktrees outside this workspace; sessions there are not managed:");
        for w in &r.outside_worktrees {
            let p = pathfmt::for_viewer(&o.cwd, Path::new(&w.path), o.home.as_deref(), false);
            println!(
                "  {}{}→  tcas workspace add {}",
                pad(&o.tilde(&w.path), 28),
                pad(w.branch.as_deref().unwrap_or(""), 20),
                shell_quote(&p)
            );
        }
    }
    for n in &r.notes {
        println!("note: {n}");
    }
    0
}

pub fn workspace_list(o: &Out, list: &[WorkspaceView]) -> i32 {
    if list.is_empty() {
        println!("no workspaces. Open one with: tcas workspace open [directory]");
        return 0;
    }
    for ws in list {
        let by = if ws.opened_by == "user" { "the user".to_string() } else { short_session(&ws.opened_by) };
        println!(
            "{}  ({})  mode {}  opened by {by}  {}  {}  {}",
            ws.name,
            ws.id,
            ws.mode,
            human::count(ws.repos.len(), "checkout", "checkouts"),
            human::count(ws.sessions, "session", "sessions"),
            human::count(ws.leases, "lease", "leases")
        );
        for r in &ws.roots {
            let added = if r.added_by == "user" { String::new() } else { format!("   added by {}", short_session(&r.added_by)) };
            println!("  {}{}{added}", pad(&r.name, 16), o.tilde(&r.path));
        }
    }
    0
}

pub fn status(o: &Out, s: &StatusResult) -> i32 {
    let ws = &s.workspace;
    let single_repo_at_root = ws.roots.len() == 1 && ws.repos.len() == 1 && ws.repos[0].dir.is_empty();
    if ws.roots.len() > 1 {
        println!("workspace \"{}\"   mode {}", ws.name, ws.mode);
        for r in &ws.roots {
            let branch = ws
                .repos
                .iter()
                .find(|x| x.root == r.name && x.dir.is_empty())
                .map(|x| repo_branch(o, x))
                .unwrap_or_default();
            println!("  {}{}{branch}", pad(&r.name, 15), pad(&o.tilde(&r.path), 28));
        }
        let nested: Vec<&RepoView> = ws.repos.iter().filter(|x| !x.dir.is_empty()).collect();
        for x in nested {
            println!("  {}{}{}", pad(&x.display, 43), pad(&x.kind, 11), repo_branch(o, x));
        }
    } else if single_repo_at_root {
        println!(
            "workspace  {}   mode {}   branch {}{}",
            o.tilde(&ws.roots[0].path),
            ws.mode,
            ws.repos[0].branch.as_deref().unwrap_or("?"),
            branch_note(o, ws.repos[0].branch_changed_at)
        );
    } else {
        println!("workspace  {}   mode {}", o.tilde(&ws.roots.first().map(|r| r.path.clone()).unwrap_or_default()), ws.mode);
        repo_lines(o, ws, None);
    }
    println!();
    if s.sessions.is_empty() {
        println!("no sessions.");
    } else {
        let user = s.viewer.is_none();
        println!(
            "{}{}{}{}LEASES",
            pad("SESSION", 16),
            pad("STATE", 8),
            pad("LAST", 7),
            pad(if user { "TITLE" } else { "BRIEF" }, 26)
        );
        for x in &s.sessions {
            let label = if user { x.title.clone() } else { x.brief.clone() }.unwrap_or_default();
            let label: String = label.chars().take(24).collect();
            let mut leases = x.leases.to_string();
            let mut extra = Vec::new();
            if x.stalled > 0 {
                extra.push(format!("{} stalled", x.stalled));
            }
            if x.orphaned > 0 {
                extra.push("orphaned".to_string());
            }
            if !x.waiting_for.is_empty() {
                extra.push(format!("waiting for {}", x.waiting_for.join(", ")));
            }
            if !extra.is_empty() {
                leases.push_str(&format!(" ({})", extra.join("; ")));
            }
            println!(
                "{}{}{}{}{leases}",
                pad(&short_session(&x.id), 16),
                pad(&x.state, 8),
                pad(&o.ago(x.last_seen_at), 7),
                pad(&label, 26)
            );
        }
    }
    for x in &s.sessions {
        let mine: Vec<&LeaseView> = s.leases.iter().filter(|l| l.session == x.id).collect();
        if mine.is_empty() {
            continue;
        }
        println!();
        let label = if s.viewer.is_none() { x.title.clone() } else { None };
        let mut head = short_session(&x.id);
        if let Some(t) = label {
            head.push_str(&format!("  {t}"));
        }
        if x.state == "ended" {
            head.push_str("   (ended; uncommitted changes remain)");
        } else if x.stalled > 0 {
            head.push_str(&format!("   (no activity for {})", human::duration(o.now - x.last_seen_at)));
        }
        println!("{head}");
        let w = width(mine.iter().map(|l| l.path.display.as_str()), 28);
        for l in &mine {
            println!("  {}{}{}   {}", pad(&l.path.display, w), pad(&l.source, 7), o.since(l.acquired_at), lease_state(o, l));
        }
        if let Some(b) = &x.brief {
            println!("  brief: {}", brief::quoted(b));
        }
        for l in mine.iter().filter(|l| l.stalled || l.state == "orphaned") {
            let p = o.arg(&l.path);
            if l.state == "orphaned" {
                println!("  → commit or discard the changes, or: tcas lease transfer {p} --to <session>");
            } else {
                println!(
                    "  → {} has uncommitted changes and will stay leased until handled:\n    go back to that session, or: tcas lease transfer {p} --to <session>",
                    l.path.display
                );
            }
        }
    }
    if !s.unattributed.is_empty() {
        println!();
        println!("unattributed changes (modified, no lease):");
        for p in &s.unattributed {
            println!("  {}", p.display);
        }
    }
    if !s.released.is_empty() {
        println!();
        println!("released (orphaned, now clean): {}", join_displays(&s.released));
    }
    if !s.outside.is_empty() {
        println!();
        println!("edited outside this workspace (not coordinated):");
        let shown: Vec<String> =
            s.outside.iter().map(|x| pathfmt::for_viewer(&o.cwd, Path::new(&x.path), o.home.as_deref(), false)).collect();
        let w = width(shown.iter().map(String::as_str), 16);
        for (x, p) in s.outside.iter().zip(&shown) {
            let who: Vec<String> = x.sessions.iter().map(|s| short_session(s)).collect();
            println!(
                "  {}{}{}→  tcas workspace add {}",
                pad(p, w),
                pad(&human::count(x.edits as usize, "edit", "edits"), 10),
                pad(&who.join(", "), 16),
                shell_quote(p)
            );
        }
    }
    0
}

pub fn session_list(o: &Out, list: &[SessionView], user: bool) -> i32 {
    if list.is_empty() {
        println!("no sessions.");
        return 0;
    }
    println!("{}{}{}{}{}{}", pad("SESSION", 40), pad("HARNESS", 9), pad("STATE", 8), pad("LAST", 7), pad("LEASES", 8), if user { "TITLE" } else { "BRIEF" });
    for s in list {
        let label = if user { s.title.clone().or(s.brief.clone()) } else { s.brief.clone() }.unwrap_or_default();
        println!(
            "{}{}{}{}{}{}",
            pad(&s.id, 40),
            pad(&s.harness, 9),
            pad(&s.state, 8),
            pad(&o.ago(s.last_seen_at), 7),
            pad(&s.leases.to_string(), 8),
            label
        );
    }
    0
}

pub fn events(evs: &[EventView]) {
    for e in evs {
        let who = e.session.as_deref().map(short_session).unwrap_or_else(|| "-".into());
        let mut p = e.payload.clone();
        let is_dir = p.get("is_prefix").and_then(|v| v.as_bool()).unwrap_or(false);
        let path = p
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| if is_dir && !s.ends_with('/') { format!("{s}/") } else { s.to_string() })
            .or_else(|| {
                p.get("paths").and_then(|v| v.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")
                })
            })
            .unwrap_or_default();
        if let Some(o) = p.as_object_mut() {
            o.remove("path");
            o.remove("paths");
            o.remove("root");
            o.remove("is_prefix");
            o.retain(|_, v| !v.is_null());
        }
        let rest = if p.as_object().is_some_and(|o| o.is_empty()) { String::new() } else { p.to_string() };
        println!("{}  {}{}{}  {rest}", clock::stamp(e.at), pad(&e.kind, 20), pad(&who, 16), path);
    }
}
