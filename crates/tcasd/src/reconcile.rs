//! Bash reconcile windows (harness.md sections 5.2 and 5.3): every checkout
//! is snapshotted before a Bash call and again after it; what changed in
//! between is attributed to the session that ran the call. Windows live in
//! memory only; a restarted tcasd loses the open ones (section 7).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde_json::json;
use tcas_core::exempt::{self, ExemptDir};
use tcas_core::lease::{self, Source};
use tcas_core::proto::Outcome;
use tcas_core::{abspath, guard, relpath, shell};
use tcas_sys::fsys;

use crate::daemon::{Daemon, R};
use crate::engine::{self, AcqReq, Env};
use crate::git::Git;
use crate::store::{self, LeaseRow};
use crate::text::{self, Viewer};
use crate::world::Ws;

/// Checkouts are discovered again when a window opens, at most this often
/// per workspace (a new worktree forces it sooner).
const DISCOVER_EVERY_MS: i64 = 30_000;
/// Changed paths handled per checkout and window.
const MAX_PATHS: usize = 300;
/// How long closed windows and edit calls are remembered for attribution.
const REMEMBER_MS: i64 = 3_600_000;

/// One checkout's state at one moment.
#[derive(Debug, Clone)]
pub struct RepoSnap {
    pub ws: String,
    pub root: String,
    /// Relative to the root.
    pub dir: String,
    pub abs: PathBuf,
    pub ci: bool,
    pub common_dir: Option<String>,
    /// Nested checkouts and exempt directories, relative to this checkout.
    /// Directories whose changes belong to another snapshot (nested checkouts,
    /// exempt directories). With `false` the path itself still counts: a
    /// submodule's recorded commit is a path of the outer repository.
    skip: Vec<(String, bool)>,
    /// `git status` ran.
    pub ok: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
    /// Changed paths relative to the checkout: (XY, size, modification time).
    pub entries: HashMap<String, (String, u64, i64)>,
    /// Linked worktrees registered in the repository.
    pub worktrees: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub agent: Option<String>,
    pub opened_at: i64,
    pub command: String,
    /// A background command (re-keyed by its task ID).
    pub background: bool,
    /// When `repos` was taken; background windows roll forward.
    pub snap_at: i64,
    pub repos: Vec<RepoSnap>,
}

#[derive(Debug, Default)]
pub struct Windows {
    /// (session, call ID or `bg:<task id>`).
    open: HashMap<(String, String), Window>,
    /// Closed windows: (session, snapshot time, closed at).
    recent: Vec<(String, i64, i64)>,
    /// Edit calls: (session, absolute path, at).
    edits: Vec<(String, String, i64)>,
    /// Workspace ID to the last discovery.
    discovered: HashMap<String, i64>,
}

impl Windows {
    fn prune(&mut self, now: i64) {
        self.recent.retain(|r| now - r.2 < REMEMBER_MS);
        self.edits.retain(|e| now - e.2 < REMEMBER_MS);
    }

    fn bg_keys(&self, sid: &str) -> Vec<String> {
        self.open.keys().filter(|(s, k)| s == sid && k.starts_with("bg:")).map(|(_, k)| k.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// Snapshots

struct Job {
    ws: String,
    root: String,
    dir: String,
    abs: PathBuf,
    ci: bool,
    common_dir: Option<String>,
    /// Directories whose changes belong to another snapshot (nested checkouts,
    /// exempt directories). With `false` the path itself still counts: a
    /// submodule's recorded commit is a path of the outer repository.
    skip: Vec<(String, bool)>,
}

fn under_skip(skip: &[(String, bool)], p: &str, ci: bool) -> bool {
    let k = relpath::key(p.trim_end_matches('/'), ci);
    skip.iter().any(|(s, itself)| relpath::contains(s, &k) && (*itself || *s != k))
}

fn take(git: &Git, j: Job) -> RepoSnap {
    let mut s = RepoSnap {
        ws: j.ws,
        root: j.root,
        dir: j.dir,
        abs: j.abs,
        ci: j.ci,
        common_dir: j.common_dir,
        skip: j.skip,
        ok: false,
        head: None,
        branch: None,
        entries: HashMap::new(),
        worktrees: BTreeSet::new(),
    };
    if let Some(st) = git.status_full(&s.abs) {
        s.ok = true;
        s.head = st.head;
        s.branch = st.branch;
        for (xy, p) in st.entries {
            if p.ends_with('/') || under_skip(&s.skip, &p, s.ci) {
                continue;
            }
            let (size, mtime) = match std::fs::symlink_metadata(s.abs.join(&p)) {
                Ok(m) => (
                    m.len(),
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0),
                ),
                Err(_) => (0, 0),
            };
            s.entries.insert(p, (xy, size, mtime));
        }
    }
    if let Some(c) = &s.common_dir
        && let Ok(rd) = std::fs::read_dir(Path::new(c).join("worktrees"))
    {
        s.worktrees = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    }
    s
}

fn run_jobs(git: &Git, jobs: Vec<Job>) -> Vec<RepoSnap> {
    let mut out = Vec::with_capacity(jobs.len());
    let mut jobs = jobs.into_iter().peekable();
    while jobs.peek().is_some() {
        let chunk: Vec<Job> = jobs.by_ref().take(16).collect();
        std::thread::scope(|sc| {
            let hs: Vec<_> = chunk.into_iter().map(|j| sc.spawn(move || take(git, j))).collect();
            out.extend(hs.into_iter().filter_map(|h| h.join().ok()));
        });
    }
    out
}

fn jobs_for(global_exempt: &[ExemptDir], ws: &Ws) -> Vec<Job> {
    let mut out = Vec::new();
    for r in &ws.repos {
        let ci = ws.ci(&r.root);
        let abs = ws.repo_abs(&r.root, &r.dir);
        let abs_s = fsys::to_str(&abs);
        if exempt::find(global_exempt, &abs_s).is_some() || exempt::find(&ws.exempt, &abs_s).is_some() {
            continue;
        }
        let dk = relpath::key(&r.dir, ci);
        // Nested checkouts snapshot their own files (harness.md section 5.2);
        // a submodule's path itself stays, as its recorded commit.
        let mut skip: Vec<(String, bool)> = ws
            .repos
            .iter()
            .filter(|o| o.root == r.root && o.dir != r.dir && relpath::contains(&dk, &relpath::key(&o.dir, ci)))
            .map(|o| (relpath::key(&relpath::strip(&r.dir, &o.dir), ci), o.kind != "submodule"))
            .collect();
        for e in global_exempt.iter().chain(ws.exempt.iter()) {
            if abspath::contains(&abs_s, &e.path, ci) && e.path.len() > abs_s.len() {
                skip.push((relpath::key(&abspath::relative(&abs_s, &e.path), ci), true));
            }
        }
        out.push(Job { ws: ws.row.id.clone(), root: r.root.clone(), dir: r.dir.clone(), abs, ci, common_dir: r.common_dir.clone(), skip });
    }
    out
}

fn resnap(git: &Git, before: &[RepoSnap]) -> Vec<RepoSnap> {
    let jobs = before
        .iter()
        .map(|b| Job {
            ws: b.ws.clone(),
            root: b.root.clone(),
            dir: b.dir.clone(),
            abs: b.abs.clone(),
            ci: b.ci,
            common_dir: b.common_dir.clone(),
            skip: b.skip.clone(),
        })
        .collect();
    run_jobs(git, jobs)
}

// ---------------------------------------------------------------------------
// Opening

/// The workspaces a Bash call may touch (harness.md section 5.2): the one
/// of its directory, those its `cd` and `-C` lead to, and those where the
/// session holds leases.
fn targets(env: &Env, sid: &str, command: &str) -> R<BTreeSet<String>> {
    let cwd = fsys::to_str(&env.cwd);
    let mut ids = BTreeSet::new();
    if let Some(ws) = env.world.by_cwd(&env.cwd) {
        ids.insert(ws.row.id.clone());
    }
    let home = tcas_sys::home::user_home().map(|h| fsys::to_str(&h));
    let parsed = shell::parse(command, &cwd, home.as_deref());
    for s in &parsed.commands {
        let mut dirs = vec![(s.dir.clone(), s.dir_known)];
        if let Some(c) = guard::git_call(s) {
            dirs.push((c.work_dir(), c.dir_known));
        }
        for (d, known) in dirs {
            if known && let Some((ws, _)) = env.world.root_of(&crate::bashguard::real_dir(&d)) {
                ids.insert(ws.row.id.clone());
            }
        }
    }
    for l in store::leases_session(env.c, sid)? {
        ids.insert(l.workspace);
    }
    ids.retain(|id| env.world.ws(id).is_some_and(|w| w.cfg.bash.reconcile));
    Ok(ids)
}

/// Finds the checkouts of these workspaces again, when it is due.
fn rediscover(env: &mut Env, ids: &BTreeSet<String>) -> R<()> {
    let now = env.now;
    let due: Vec<String> = {
        let w = env.d.windows();
        ids.iter().filter(|id| w.discovered.get(*id).is_none_or(|t| now - t >= DISCOVER_EVERY_MS)).cloned().collect()
    };
    if due.is_empty() {
        return Ok(());
    }
    for id in &due {
        let Some(ws) = env.world.ws(id).cloned() else { continue };
        for r in &ws.roots {
            let d = crate::handlers::workspace::discover(env, Path::new(&r.path), r.ci, ws.cfg.repos.scan_depth, &ws.exempt, &[]);
            crate::handlers::workspace::store_repos(env, id, &r.name, &d.found, false)?;
        }
        env.d.windows().discovered.insert(id.clone(), now);
    }
    env.reload()
}

/// The branch last recorded for a checkout.
fn last_branch(env: &Env, ws: &str, root: &str, dir: &str) -> R<Option<String>> {
    Ok(env
        .c
        .query_row(
            "SELECT last_branch FROM repo WHERE workspace=?1 AND root=?2 AND dir=?3",
            params![ws, root, dir],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Opens a window for a Bash call (`PreToolUse`, after the guard).
pub fn open(env: &mut Env, sid: &str, agent: Option<&str>, call_id: &str, command: &str) -> R<()> {
    let cwd = fsys::to_str(&env.cwd);
    if exempt::find(&env.g.exempt, &cwd).is_some()
        || env.world.root_of(&cwd).is_some_and(|(ws, _)| exempt::find(&ws.exempt, &cwd).is_some())
    {
        return Ok(());
    }
    let ids = targets(env, sid, command)?;
    if ids.is_empty() {
        return Ok(());
    }
    rediscover(env, &ids)?;
    let jobs: Vec<Job> = ids.iter().filter_map(|id| env.world.ws(id)).flat_map(|ws| jobs_for(&env.g.exempt, ws)).collect();
    let snaps = run_jobs(&env.d.git, jobs);
    // A branch that changed outside every window, probably by the user.
    for s in &snaps {
        if !s.ok || s.branch.is_none() {
            continue;
        }
        let last = last_branch(env, &s.ws, &s.root, &s.dir)?;
        if last != s.branch
            && let Some(ws) = env.world.ws(&s.ws)
        {
            if last.is_some() {
                branch_changed(env, ws, &s.root, &s.dir, last.as_deref(), s.branch.as_deref(), None)?;
            } else {
                set_branch(env, &s.ws, &s.root, &s.dir, s.branch.as_deref(), None)?;
            }
        }
    }
    let one: String = command.chars().take(1000).collect();
    let mut w = env.d.windows();
    w.prune(env.now);
    w.open.insert(
        (sid.to_string(), call_id.to_string()),
        Window { agent: agent.map(str::to_string), opened_at: env.now, command: one, background: false, snap_at: env.now, repos: snaps },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Closing

/// What one request has already reported, so a change seen by several
/// windows of the same session is reported once.
#[derive(Default)]
pub struct Reported {
    seen: HashSet<(String, String)>,
}

/// Closes a foreground or completed background window (`PostToolUse`) and
/// reconciles it. Returns texts for the session.
pub fn close(env: &Env, sid: &str, call_id: &str, rep: &mut Reported) -> R<Vec<String>> {
    let w = {
        let mut windows = env.d.windows();
        windows
            .open
            .remove(&(sid.to_string(), call_id.to_string()))
            .or_else(|| windows.open.remove(&(sid.to_string(), format!("bg:{call_id}"))))
    };
    let Some(w) = w else { return Ok(vec![]) };
    let after = resnap(&env.d.git, &w.repos);
    let texts = reconcile(env, sid, &w, &after, rep)?;
    env.d.windows().recent.push((sid.to_string(), w.snap_at, env.now));
    Ok(texts)
}

/// Promotes open main-agent foreground windows of `sid` into background windows
/// keyed by `bg:<call_id>` (for harnesses like AGY where `Stop(fullyIdle=false)`
/// fires while async commands are still running and `PostToolUse` fires on exit).
pub fn promote_foreground_bash(d: &Daemon, sid: &str) {
    let mut w = d.windows();
    let keys: Vec<String> = w
        .open
        .iter()
        .filter(|((s, k), win)| s == sid && !k.starts_with("bg:") && win.agent.is_none())
        .map(|((_, k), _)| k.clone())
        .collect();
    for k in keys {
        if let Some(mut win) = w.open.remove(&(sid.to_string(), k.clone())) {
            win.background = true;
            w.open.insert((sid.to_string(), format!("bg:{k}")), win);
        }
    }
}

/// A background call started: its window stays open under the task ID
/// (harness.md section 5.3).
pub fn to_background(d: &Daemon, sid: &str, call_id: &str, task: &str) {
    let mut w = d.windows();
    if let Some(mut win) = w.open.remove(&(sid.to_string(), call_id.to_string())) {
        win.background = true;
        w.open.insert((sid.to_string(), format!("bg:{task}")), win);
    }
}

/// Reconciles the session's background windows and rolls them forward
/// (at the end of each of its Bash calls).
pub fn roll_background(env: &Env, sid: &str, rep: &mut Reported) -> R<Vec<String>> {
    let keys = env.d.windows().bg_keys(sid);
    let mut texts = Vec::new();
    for k in keys {
        let key = (sid.to_string(), k);
        let Some(mut win) = env.d.windows().open.remove(&key) else { continue };
        let after = resnap(&env.d.git, &win.repos);
        let r = reconcile(env, sid, &win, &after, rep);
        win.repos = after;
        win.snap_at = env.now;
        env.d.windows().open.insert(key, win);
        texts.extend(r?);
    }
    Ok(texts)
}

/// Closes the background windows whose commands are no longer running
/// (`Stop`; `running` is `None` when the harness does not say).
pub fn close_background(env: &Env, sid: &str, running: Option<&[String]>, rep: &mut Reported) -> R<Vec<String>> {
    let still = |id: &str| running.is_some_and(|r| r.iter().any(|x| x == id));
    let keys = env.d.windows().bg_keys(sid);
    let mut texts = Vec::new();
    for k in keys {
        let id = k.trim_start_matches("bg:").to_string();
        if still(&id) {
            continue;
        }
        let Some(win) = env.d.windows().open.remove(&(sid.to_string(), k)) else { continue };
        let after = resnap(&env.d.git, &win.repos);
        texts.extend(reconcile(env, sid, &win, &after, rep)?);
        env.d.windows().recent.push((sid.to_string(), win.snap_at, env.now));
    }
    let ids: Vec<String> = {
        let mut st = env.c.prepare("SELECT background_id FROM open_call WHERE session=?1 AND background_id IS NOT NULL")?;
        st.query_map([sid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    for id in ids {
        if !still(&id) {
            env.c.execute("DELETE FROM open_call WHERE session=?1 AND background_id=?2", params![sid, id])?;
        }
    }
    Ok(texts)
}

/// Closes the session's foreground windows of `agent` (the main agent with
/// `None`) whose calls never reported back.
pub fn close_foreground(env: &Env, sid: &str, agent: Option<&str>, rep: &mut Reported) -> R<Vec<String>> {
    let keys: Vec<String> = env
        .d
        .windows()
        .open
        .iter()
        .filter(|((s, k), w)| s == sid && !k.starts_with("bg:") && w.agent.as_deref() == agent)
        .map(|((_, k), _)| k.clone())
        .collect();
    let mut texts = Vec::new();
    for k in keys {
        texts.extend(close(env, sid, &k, rep)?);
    }
    Ok(texts)
}

/// Forgets a session's windows (it ended).
pub fn drop_session(d: &Daemon, sid: &str) {
    d.windows().open.retain(|(s, _), _| s != sid);
}

/// An edit call by `sid` on these paths: a change to them in another
/// session's window is the editor's own (harness.md section 5.2).
pub fn edited(d: &Daemon, sid: &str, paths: &[String], now: i64) {
    let mut w = d.windows();
    w.prune(now);
    for p in paths {
        w.edits.push((sid.to_string(), p.clone(), now));
    }
}

/// Sessions other than `sid` whose Bash windows overlapped the time since
/// `since` (background ones only for their first `window_ms`).
fn others_running(d: &Daemon, sid: &str, since: i64, now: i64, window_ms: i64) -> HashSet<String> {
    let w = d.windows();
    let mut out = HashSet::new();
    for ((s, _), win) in &w.open {
        if s != sid && (!win.background || now - win.opened_at < window_ms) {
            out.insert(s.clone());
        }
    }
    for (s, _, closed) in &w.recent {
        if s != sid && *closed >= since {
            out.insert(s.clone());
        }
    }
    out
}

fn edits_since(d: &Daemon, since: i64) -> Vec<(String, String)> {
    d.windows().edits.iter().filter(|e| e.2 >= since).map(|e| (e.0.clone(), e.1.clone())).collect()
}

/// The paths a moved `HEAD` brought: each new commit's own paths, except
/// commits another session reported; a reset or switch gives the whole
/// difference. New commits nobody reported are processed as this
/// session's (no git hooks).
fn head_paths(env: &Env, sid: &str, ws: &Ws, b: &RepoSnap, a: &RepoSnap, out: &mut BTreeSet<String>) -> R<()> {
    let git = &env.d.git;
    let commit = |c: &str, out: &mut BTreeSet<String>| -> R<()> {
        let seen: Option<Option<String>> = env
            .c
            .query_row(
                "SELECT by FROM commit_seen WHERE workspace=?1 AND root=?2 AND repo=?3 AND hash=?4",
                params![ws.row.id, b.root, b.dir, c],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        let paths = git.commit_paths(&a.abs, c);
        match seen {
            Some(by) if by.as_deref() != Some(sid) => return Ok(()),
            Some(_) => {}
            None => {
                crate::handlers::git::process_commit(env, ws, &b.root, &b.dir, c, &paths, Some(sid))?;
            }
        }
        out.extend(paths);
        Ok(())
    };
    match (&b.head, &a.head) {
        (Some(o), Some(n)) if o != n => {
            if git.rev_list(&a.abs, n, o).is_empty() {
                for c in git.rev_list(&a.abs, o, n) {
                    commit(&c, out)?;
                }
            } else {
                out.extend(git.diff_names(&a.abs, o, n));
            }
        }
        (None, Some(n)) => commit(n, out)?,
        _ => {}
    }
    Ok(())
}

/// The directory of a linked worktree registered as `name`.
fn worktree_dir(common_dir: &str, name: &str) -> Option<String> {
    let gitdir = std::fs::read_to_string(Path::new(common_dir).join("worktrees").join(name).join("gitdir")).ok()?;
    let p = PathBuf::from(gitdir.trim());
    let dir = p.parent()?.to_path_buf();
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    Some(fsys::to_str(&dir))
}

/// What one window saw, handled (harness.md section 5.2, "关闭窗口").
fn reconcile(env: &Env, sid: &str, w: &Window, after: &[RepoSnap], rep: &mut Reported) -> R<Vec<String>> {
    let since = w.snap_at;
    let window_ms = env
        .world
        .by_cwd(&env.cwd)
        .or_else(|| w.repos.first().and_then(|r| env.world.ws(&r.ws)))
        .map(|ws| ws.cfg.bash.background_window_secs as i64 * 1000)
        .unwrap_or(60_000);
    let running = others_running(env.d, sid, since, env.now, window_ms);
    let edits = edits_since(env.d, since);
    let v = Viewer::new(&env.cwd, env.now);

    let mut acquire: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // (workspace, holder) to the paths.
    let mut overreach: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut uncertain: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut unattributed: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    let mut outside_worktrees: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();

    for b in &w.repos {
        let Some(a) = after.iter().find(|a| a.ws == b.ws && a.root == b.root && a.dir == b.dir) else { continue };
        if !(a.ok && b.ok) {
            continue;
        }
        let Some(ws) = env.world.ws(&b.ws) else { continue };
        let mut changed: BTreeSet<String> = BTreeSet::new();
        for (p, e) in &a.entries {
            if b.entries.get(p) != Some(e) {
                changed.insert(p.clone());
            }
        }
        for p in b.entries.keys() {
            if !a.entries.contains_key(p) {
                changed.insert(p.clone());
            }
        }
        if a.head != b.head {
            head_paths(env, sid, ws, b, a, &mut changed)?;
        }
        // The branch, against what was last recorded: a change another
        // window already reported is not reported again.
        if a.branch.is_some() {
            let last = last_branch(env, &b.ws, &b.root, &b.dir)?.or_else(|| b.branch.clone());
            if last != a.branch {
                branch_changed(env, ws, &b.root, &b.dir, last.as_deref(), a.branch.as_deref(), Some(sid))?;
            }
        }
        if let Some(c) = &a.common_dir {
            let new: Vec<&String> = a.worktrees.difference(&b.worktrees).collect();
            if !new.is_empty() {
                env.d.windows().discovered.remove(&b.ws);
            }
            for n in new {
                if let Some(dir) = worktree_dir(c, n)
                    && env.world.root_of(&dir).is_none()
                    && exempt::find(&env.g.exempt, &dir).is_none()
                    && !outside_worktrees.contains(&dir)
                {
                    outside_worktrees.push(dir);
                }
            }
        }
        changed.retain(|p| !under_skip(&a.skip, p, a.ci));
        if changed.is_empty() {
            continue;
        }
        if changed.len() > MAX_PATHS {
            env.event(Some(&b.ws), Some(sid), "bash.truncated", json!({ "repo": b.dir, "paths": changed.len() }))?;
        }
        let leases: Vec<LeaseRow> = store::leases_ws(env.c, &b.ws)?.into_iter().filter(|l| l.root == b.root).collect();
        for p in changed.iter().take(MAX_PATHS) {
            let rel = relpath::join(&b.dir, p);
            let key = relpath::key(&rel, b.ci);
            let abs = ws.abs(&b.root, &rel);
            if exempt::find(&env.g.exempt, &abs).is_some()
                || exempt::find(&ws.exempt, &abs).is_some()
                || ws.find_unmanaged(&b.root, &rel).is_some()
            {
                continue;
            }
            let dirty = a.entries.contains_key(p);
            let covering: Vec<&LeaseRow> =
                leases.iter().filter(|l| lease::covers(&l.path_key, l.is_prefix, &key, false)).collect();
            let shown = ws.display(&b.root, &rel, false);
            if covering.iter().any(|l| l.session == sid) {
                if dirty {
                    acquire.entry(b.ws.clone()).or_default().push(abs);
                }
            } else if let Some(t) = covering.first().map(|l| l.session.clone()) {
                let same = |e: &str| if b.ci { e.eq_ignore_ascii_case(&abs) } else { e == abs };
                if edits.iter().any(|(s, e)| *s == t && same(e)) {
                    continue;
                }
                if !rep.seen.insert((t.clone(), abs.clone())) {
                    continue;
                }
                let into = if running.contains(&t) { &mut uncertain } else { &mut overreach };
                into.entry((b.ws.clone(), t)).or_default().push(shown);
            } else if dirty {
                if running.is_empty() {
                    acquire.entry(b.ws.clone()).or_default().push(abs);
                } else {
                    unattributed.entry(b.ws.clone()).or_default().push((b.root.clone(), rel, shown));
                }
            }
        }
    }

    // Grants: the session changed these paths (harness.md section 5.2).
    for (wid, inputs) in &acquire {
        let Some(ws) = env.world.ws(wid) else { continue };
        let acq = engine::acquire(env, &bash_req(sid, inputs))?;
        let mut granted = Vec::new();
        if acq.result.outcome == Outcome::Granted {
            granted = acq.result.granted;
        } else {
            for i in inputs {
                let one = [i.clone()];
                let r = engine::acquire(env, &bash_req(sid, &one))?;
                if r.result.outcome == Outcome::Granted {
                    granted.extend(r.result.granted);
                }
            }
        }
        for g in &granted {
            env.c.execute(
                "DELETE FROM unattributed WHERE workspace=?1 AND root=?2 AND path=?3",
                params![wid, g.path.root, g.path.path],
            )?;
        }
        let new: Vec<String> = granted.iter().filter(|g| !g.renewed).map(|g| g.path.display.clone()).collect();
        if !new.is_empty() && ws.mode(env.g.cfg.defaults.mode) == tcas_core::config::Mode::Strict {
            texts.push(text::bash_strict(&new));
        }
    }

    let command = &w.command;
    if !overreach.is_empty() {
        let mut rows: Vec<(String, String)> = Vec::new();
        for ((wid, t), paths) in &overreach {
            env.event(Some(wid), Some(sid), "bash.overreach", json!({ "holder": t, "paths": paths, "command": command }))?;
            env.notice(t, "bash.overreach", json!({ "workspace": wid, "by": sid, "paths": paths, "command": command }))?;
            let b = holder_brief(env, t, wid)?;
            for p in paths {
                rows.push((p.clone(), format!("{}{b}", tcas_core::human::short_session(t))));
            }
        }
        texts.push(text::overreach_self(&rows));
    }
    if !uncertain.is_empty() {
        let mut rows: Vec<(String, String)> = Vec::new();
        for ((wid, t), paths) in &uncertain {
            env.event(Some(wid), Some(sid), "bash.unattributed", json!({ "holder": t, "paths": paths, "command": command }))?;
            env.notice(t, "bash.uncertain", json!({ "workspace": wid, "by": sid, "paths": paths, "command": command }))?;
            for p in paths {
                rows.push((p.clone(), tcas_core::human::short_session(t)));
            }
        }
        texts.push(text::uncertain_self(&rows));
    }
    if !unattributed.is_empty() {
        let mut shown = Vec::new();
        for (wid, items) in &unattributed {
            for (root, rel, s) in items {
                env.c.execute(
                    "INSERT OR REPLACE INTO unattributed(workspace, root, path, seen_at) VALUES(?1,?2,?3,?4)",
                    params![wid, root, rel, env.now],
                )?;
                shown.push(s.clone());
            }
            env.event(
                Some(wid),
                Some(sid),
                "bash.unattributed",
                json!({ "paths": items.iter().map(|i| &i.2).collect::<Vec<_>>(), "command": command }),
            )?;
        }
        texts.push(text::unattributed_self(&shown));
    }
    for d in outside_worktrees {
        env.event(env.world.by_cwd(&env.cwd).map(|w| w.row.id.as_str()), Some(sid), "worktree.outside", json!({ "path": d }))?;
        texts.push(text::worktree_outside(&v.path(&d, true)));
    }
    Ok(texts)
}

fn bash_req<'a>(sid: &'a str, inputs: &'a [String]) -> AcqReq<'a> {
    AcqReq { session: sid, inputs, source: Source::Bash, brief: None, check_size: false, record_denial: false }
}

/// ` "brief"` of a holder: its latest brief, for the overreach text.
fn holder_brief(env: &Env, holder: &str, _ws: &str) -> R<String> {
    let b: Option<String> = env
        .c
        .query_row("SELECT brief FROM session WHERE id=?1", [holder], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(b.map(|b| format!(" {}", tcas_core::brief::quoted(&b))).unwrap_or_default())
}

fn set_branch(env: &Env, ws: &str, root: &str, dir: &str, branch: Option<&str>, changed_at: Option<i64>) -> R<()> {
    if let Some(at) = changed_at {
        env.c.execute(
            "UPDATE repo SET last_branch=?4, branch_changed_at=?5 WHERE workspace=?1 AND root=?2 AND dir=?3",
            params![ws, root, dir, branch, at],
        )?;
    } else {
        env.c.execute(
            "UPDATE repo SET last_branch=?4 WHERE workspace=?1 AND root=?2 AND dir=?3",
            params![ws, root, dir, branch],
        )?;
    }
    Ok(())
}

/// A checkout's branch changed (harness.md section 5.2, "分支变了"): an
/// event, and a notice to every other session holding leases in it.
pub fn branch_changed(
    env: &Env,
    ws: &Ws,
    root: &str,
    dir: &str,
    from: Option<&str>,
    to: Option<&str>,
    by: Option<&str>,
) -> R<()> {
    set_branch(env, &ws.row.id, root, dir, to, Some(env.now))?;
    let repo = repo_shown(ws, root, dir);
    env.event(
        Some(&ws.row.id),
        by,
        "branch.changed",
        json!({ "root": root, "dir": dir, "repo": repo, "from": from, "to": to, "by": by }),
    )?;
    for h in holders_in(env, ws, root, dir)? {
        if Some(h.as_str()) != by {
            env.notice(&h, "branch.changed", json!({ "workspace": ws.row.id, "repo": repo, "from": from, "to": to, "by": by }))?;
        }
    }
    Ok(())
}

/// How a checkout is named in texts: `web/`, or `.` for a lone root.
pub fn repo_shown(ws: &Ws, root: &str, dir: &str) -> String {
    if dir.is_empty() && !ws.multi_root() { ".".into() } else { ws.display(root, dir, true) }
}

/// Sessions holding leases in a checkout (directory leases spanning it count).
pub fn holders_in(env: &Env, ws: &Ws, root: &str, dir: &str) -> R<BTreeSet<String>> {
    let ci = ws.ci(root);
    let dk = relpath::key(dir, ci);
    Ok(store::leases_ws(env.c, &ws.row.id)?
        .into_iter()
        .filter(|l| l.root == root)
        .filter(|l| match &l.repo {
            Some(r) => relpath::key(r, ci) == dk,
            None => l.is_prefix && (relpath::contains(&l.path_key, &dk) || relpath::contains(&dk, &l.path_key)),
        })
        .map(|l| l.session)
        .collect())
}
