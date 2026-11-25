//! `workspace.*` (daemon-and-cli.md section 5, "workspace").

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use tcas_core::config::Mode;
use tcas_core::exempt;
use tcas_core::pathfmt;
use tcas_core::proto::{OpenResult, OutsideWorktree, RepoView, Request, RootView, WorkspaceView, code};
use tcas_core::{abspath, relpath};
use tcas_sys::{fsys, home::user_home};

use super::args;
use crate::daemon::{Caller, Fail, R};
use crate::engine::{self, Env};
use crate::githooks;
use crate::repos::{self, Found};
use crate::store::{self, RootRow};
use crate::world::Ws;

fn tilde(p: &str) -> String {
    pathfmt::tilde(Path::new(p), user_home().as_deref())
}

pub fn view(env: &Env, ws: &Ws) -> R<WorkspaceView> {
    let leases = store::leases_ws(env.c, &ws.row.id)?;
    let sessions = members(env, ws)?;
    Ok(WorkspaceView {
        id: ws.row.id.clone(),
        name: ws.row.name.clone(),
        mode: ws.mode(env.g.cfg.defaults.mode).as_str().into(),
        opened_by: ws.row.opened_by.clone(),
        created_at: ws.row.created_at,
        roots: ws
            .roots
            .iter()
            .map(|r| RootView { name: r.name.clone(), path: r.path.clone(), case_insensitive: r.ci, added_by: r.added_by.clone() })
            .collect(),
        repos: ws
            .repos
            .iter()
            .map(|r| RepoView {
                root: r.root.clone(),
                dir: r.dir.clone(),
                display: ws.display(&r.root, &r.dir, true),
                kind: r.kind.clone(),
                branch: r.last_branch.clone(),
                branch_changed_at: r.branch_changed_at,
                abs: ws.abs(&r.root, &r.dir),
            })
            .collect(),
        sessions: sessions.iter().filter(|s| s.1 != "ended").count(),
        leases: leases.len(),
    })
}

/// Sessions belonging to a workspace: their cwd is in a root, or they hold
/// leases in it. Returns (id, state).
pub fn members(env: &Env, ws: &Ws) -> R<Vec<(String, String)>> {
    let holders: HashSet<String> = store::leases_ws(env.c, &ws.row.id)?.into_iter().map(|l| l.session).collect();
    let mut out = Vec::new();
    for s in store::sessions(env.c)? {
        let in_root = s.cwd.as_deref().is_some_and(|c| ws.roots.iter().any(|r| abspath::contains(&r.path, c, r.ci)));
        if in_root && s.state != "ended" || holders.contains(&s.id) {
            out.push((s.id, s.state));
        }
    }
    Ok(out)
}

fn short_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(tcas_sys::clock::now_ms());
    h.write_u32(std::process::id());
    format!("{:06x}", h.finish() & 0xff_ffff)
}

fn unique(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken.iter().any(|t| t == n)).unwrap()
}

fn base_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into())
}

/// Checks a directory can become a root (daemon-and-cli.md section 5).
/// `same_ws` is the workspace it is being added to, if any; returning
/// `Ok(Some(ws id))` means it already is a root of that workspace.
fn check_root(env: &Env, dir: &Path, same_ws: Option<&str>) -> R<Option<String>> {
    let s = fsys::to_str(dir);
    if let Some(e) = exempt::find(&env.g.exempt, &s) {
        let fix = if e.source.starts_with("built-in") {
            "To manage it anyway, set  [exempt] builtin = false  in ~/.tcas/config.toml.".to_string()
        } else {
            format!("To manage it anyway, remove it:  tcas exempt remove {}", pathfmt::shell_quote(&e.written))
        };
        return Err(Fail::refused(format!(
            "{} is under {}, which is exempt ({}).\nTCAS does not manage exempt directories. {fix}",
            tilde(&s),
            e.written,
            e.source
        )));
    }
    for ws in &env.world.wss {
        for r in &ws.roots {
            let ci = r.ci;
            let inside = abspath::contains(&r.path, &s, ci);
            let around = abspath::contains(&s, &r.path, ci);
            if inside && around {
                if same_ws == Some(ws.row.id.as_str()) || same_ws.is_none() {
                    return Ok(Some(ws.row.id.clone()));
                }
                return Err(Fail::refused(format!(
                    "{} is already a root of workspace \"{}\".",
                    tilde(&s),
                    ws.row.name
                )));
            }
            if inside || around {
                let rel = if inside { "inside" } else { "around" };
                return Err(Fail::refused(format!(
                    "{} is {rel} {}, a root of workspace \"{}\". Roots cannot contain each other.{}",
                    tilde(&s),
                    tilde(&r.path),
                    ws.row.name,
                    if around {
                        " To open the parent directory instead, close that workspace first."
                    } else {
                        ""
                    }
                )));
            }
        }
    }
    if let Some(top) = env.d.git.toplevel(dir)
        && top != dir
    {
        return Err(Fail::refused(format!(
            "{} is inside the checkout {}, but is not its root. A root is a checkout's root, or a directory \
             outside any checkout. Open {} instead.",
            tilde(&s),
            tilde(&fsys::to_str(&top)),
            tilde(&fsys::to_str(&top)),
        )));
    }
    Ok(None)
}

fn canonical(env: &Env, input: &str) -> R<PathBuf> {
    let p = fsys::absolute(&env.cwd, Path::new(input));
    fsys::canonical_dir(&p).map_err(|e| Fail::bad(format!("{input}: {e}")))
}

pub struct Discovered {
    pub found: Vec<Found>,
    pub outside: Vec<(PathBuf, Option<String>)>,
}

pub fn discover(env: &Env, dir: &Path, ci: bool, depth: usize, ws_exempt: &[tcas_core::exempt::ExemptDir], extra_roots: &[(String, bool)]) -> Discovered {
    let mut all_roots: Vec<(String, bool)> =
        env.world.wss.iter().flat_map(|w| w.roots.iter().map(|r| (r.path.clone(), r.ci))).collect();
    all_roots.extend(extra_roots.iter().cloned());
    let mut ex = env.g.exempt.clone();
    ex.extend(ws_exempt.iter().cloned());
    let d = repos::discover(&env.d.git, dir, ci, depth, &ex, &all_roots);
    Discovered { found: d.found, outside: d.outside }
}

/// tcas's git hooks go into this workspace's checkouts (harness.md section 5.6).
pub fn hooks_wanted(env: &Env, ws: &Ws) -> bool {
    !ws.row.no_git_hooks && ws.cfg.git_hooks.enabled && githooks::supported(&env.d.git)
}

/// Replaces the repo rows of one root, with events for what appeared or
/// went. Hooks are installed in new checkouts, or in all of them with
/// `install_all` (opening and adding, which the user asked for). A branch
/// that differs from the one last seen is reported (harness.md section 5.2).
pub fn store_repos(env: &Env, ws_id: &str, root: &str, found: &[Found], install_all: bool) -> R<()> {
    let ws = env.world.ws(ws_id);
    let old: Vec<store::RepoRow> =
        store::repos(env.c, ws_id)?.into_iter().filter(|r| r.root == root).collect();
    let new_dirs: BTreeSet<&str> = found.iter().map(|f| f.dir.as_str()).collect();
    for o in &old {
        if !new_dirs.contains(o.dir.as_str()) {
            if let Some(ws) = ws {
                super::repo_gone(env, ws, root, &o.dir)?;
            } else {
                env.c.execute(
                    "DELETE FROM repo WHERE workspace=?1 AND root=?2 AND dir=?3",
                    params![ws_id, root, o.dir],
                )?;
            }
        }
    }
    let hooks = ws.is_some_and(|w| hooks_wanted(env, w));
    for f in found {
        let before = old.iter().find(|o| o.dir == f.dir);
        let prev_changed = before.and_then(|o| o.branch_changed_at);
        env.c.execute(
            "INSERT OR REPLACE INTO repo(workspace, root, dir, kind, common_dir, last_branch, branch_changed_at, seen_at) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![ws_id, root, f.dir, f.kind, f.common_dir, f.branch, prev_changed, env.now],
        )?;
        if let (Some(ws), Some(o)) = (ws, before)
            && o.last_branch.is_some()
            && f.branch.is_some()
            && o.last_branch != f.branch
        {
            crate::reconcile::branch_changed(env, ws, root, &f.dir, o.last_branch.as_deref(), f.branch.as_deref(), None)?;
        }
        if before.is_none() {
            env.event(Some(ws_id), env.caller.session(), "repo.found", json!({ "root": root, "dir": f.dir, "kind": f.kind }))?;
        }
        if hooks
            && (install_all || before.is_none())
            && let Some(ws) = ws
        {
            let abs = ws.repo_abs(root, &f.dir);
            match githooks::install(&env.d.git, &env.d.home, &abs) {
                Ok(true) => env.event(Some(ws_id), None, "git_hooks.installed", json!({ "repo": fsys::to_str(&abs) }))?,
                Ok(false) => {}
                Err(e) => crate::daemon::log(format!("git hooks in {}: {e}", abs.display())),
            }
        }
    }
    Ok(())
}

/// Takes tcas's hooks out of checkouts no longer in any workspace. A
/// repository's checkouts share one configuration, so the line stays while
/// another of them is still in a workspace (harness.md section 5.6). Call
/// after the rows are gone.
pub fn remove_hooks(env: &Env, gone: &[(PathBuf, Option<String>)]) -> R<()> {
    let remaining: HashSet<String> = store::all_repos(env.c)?.into_iter().filter_map(|r| r.common_dir).collect();
    let mut done: HashSet<String> = HashSet::new();
    for (abs, common) in gone {
        if let Some(c) = common
            && (remaining.contains(c) || !done.insert(c.clone()))
        {
            continue;
        }
        let target = if abs.join(".git").exists() {
            abs.clone()
        } else if let Some(c) = common.as_ref().filter(|c| Path::new(c).is_dir()) {
            PathBuf::from(c)
        } else {
            continue;
        };
        match githooks::remove(&env.d.git, &env.d.home, &target) {
            Ok(true) => env.event(None, None, "git_hooks.removed", json!({ "repo": fsys::to_str(abs) }))?,
            Ok(false) => {}
            Err(e) => crate::daemon::log(format!("git hooks in {}: {e}", abs.display())),
        }
    }
    Ok(())
}

fn repo_paths(ws: &Ws, root: Option<&str>) -> Vec<(PathBuf, Option<String>)> {
    ws.repos
        .iter()
        .filter(|r| root.is_none_or(|n| r.root == n))
        .map(|r| (ws.repo_abs(&r.root, &r.dir), r.common_dir.clone()))
        .collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpenArgs {
    paths: Vec<String>,
    strict: bool,
    no_git_hooks: bool,
}

fn agent_open_checks(env: &Env, a: &OpenArgs, dirs: &[PathBuf]) -> R<()> {
    let Caller::Session(_) = &env.caller else { return Ok(()) };
    if !env.g.cfg.agents.workspace_open {
        return Err(Fail::user_only("Sessions cannot open workspaces here ([agents] workspace_open = false)."));
    }
    if a.strict || a.no_git_hooks {
        return Err(Fail::user_only("A session cannot open a workspace with --strict or --no-git-hooks."));
    }
    let home = user_home().and_then(|h| std::fs::canonicalize(h).ok());
    if dirs.iter().any(|d| d == Path::new("/") || Some(d) == home.as_ref()) {
        return Err(Fail::user_only("A session cannot open the home directory or the filesystem root."));
    }
    let allowed = env.d.git.toplevel(&env.cwd).unwrap_or_else(|| env.cwd.clone());
    if dirs.len() != 1 || dirs[0] != allowed {
        return Err(Fail::user_only(&format!(
            "A session can open only its current checkout or directory ({}).",
            tilde(&fsys::to_str(&allowed))
        )));
    }
    Ok(())
}

pub fn open(env: &mut Env, req: &Request) -> R<Value> {
    let a: OpenArgs = args(req)?;
    let mut dirs = Vec::new();
    if a.paths.is_empty() {
        dirs.push(env.d.git.toplevel(&env.cwd).unwrap_or_else(|| env.cwd.clone()));
    } else {
        for p in &a.paths {
            let d = canonical(env, p)?;
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    agent_open_checks(env, &a, &dirs)?;

    // Already open: refresh the checkouts and report.
    let mut existing: Option<String> = None;
    for d in &dirs {
        match check_root(env, d, None)? {
            Some(id) => {
                if existing.as_ref().is_some_and(|e| e != &id) {
                    return Err(Fail::refused("these directories are roots of different workspaces"));
                }
                existing = Some(id);
            }
            None if existing.is_some() => {
                return Err(Fail::refused(format!(
                    "{} is already open; add the other directories with tcas workspace add",
                    tilde(&fsys::to_str(&dirs[0]))
                )));
            }
            None => {}
        }
    }
    if let Some(id) = existing {
        if dirs.len() > 1 && dirs.iter().any(|d| env.world.root_of(&fsys::to_str(d)).is_none()) {
            return Err(Fail::refused("some of these directories are already open; add the rest with tcas workspace add"));
        }
        let ws = env.world.ws(&id).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
        let mut outside = Vec::new();
        for r in &ws.roots {
            let found = discover(env, Path::new(&r.path), r.ci, ws.cfg.repos.scan_depth, &ws.exempt, &[]);
            store_repos(env, &id, &r.name, &found.found, true)?;
            outside.extend(found.outside);
        }
        env.reload()?;
        let ws = env.world.ws(&id).cloned().unwrap();
        let res = OpenResult {
            workspace: view(env, &ws)?,
            added: vec![],
            already_open: true,
            outside_worktrees: outside_view(&outside),
            notes: vec![],
        };
        return Ok(serde_json::to_value(res).unwrap());
    }
    // Several new dirs must not nest among themselves either.
    for (i, a1) in dirs.iter().enumerate() {
        for a2 in &dirs[i + 1..] {
            if a1.starts_with(a2) || a2.starts_with(a1) {
                return Err(Fail::refused(format!(
                    "{} and {} contain one another; roots cannot nest",
                    tilde(&fsys::to_str(a1)),
                    tilde(&fsys::to_str(a2))
                )));
            }
        }
    }

    // Workspace config from the first root, including extra roots.
    let first = dirs[0].clone();
    let cfg = std::fs::read_to_string(first.join(".tcas.toml"))
        .ok()
        .and_then(|t| tcas_core::config::parse_workspace(&t).ok())
        .unwrap_or_default();
    let mut notes = Vec::new();
    for extra in &cfg.roots {
        let p = fsys::absolute(&first, Path::new(extra));
        match fsys::canonical_dir(&p) {
            Ok(d) if !dirs.contains(&d) => match check_root(env, &d, None) {
                Ok(None) => dirs.push(d),
                Ok(Some(_)) => notes.push(format!("roots: {extra} is already a root of another workspace; skipped")),
                Err(e) => notes.push(format!("roots: {extra}: {}", e.message)),
            },
            Ok(_) => {}
            Err(e) => notes.push(format!("roots: {extra}: {e}")),
        }
    }

    // Discover before writing anything, for the agent's checkout limit.
    let ws_exempt_first = tcas_sys::exempt::workspace_list(&first, &cfg.exempt, "workspace", fsys::case_insensitive(&first));
    let extra_roots: Vec<(String, bool)> = dirs.iter().map(|d| (fsys::to_str(d), fsys::case_insensitive(d))).collect();
    let mut discovered = Vec::new();
    for d in &dirs {
        let ci = fsys::case_insensitive(d);
        discovered.push(discover(env, d, ci, cfg.repos.scan_depth, &ws_exempt_first, &extra_roots));
    }
    if !env.caller.is_user() {
        let n: usize = discovered.iter().map(|x| x.found.len()).sum();
        let max = env.g.cfg.agents.open_max_checkouts;
        if n > max {
            return Err(Fail::user_only(&format!(
                "This directory holds {n} checkouts; a session may open at most {max}."
            )));
        }
    }

    let id = short_id();
    let taken: Vec<String> = env.world.wss.iter().map(|w| w.row.name.clone()).collect();
    let name = unique(&base_name(&first), &taken);
    let mode = if a.strict { Some(Mode::Strict.as_str().to_string()) } else { None };
    let git_hooks: Option<i64> = if a.no_git_hooks { Some(0) } else { None };
    env.c.execute(
        "INSERT INTO workspace(id, name, mode, opened_by, created_at, git_hooks) VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, name, mode, env.caller.label(), env.now, git_hooks],
    )?;
    let mut root_names: Vec<String> = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let rname = unique(&base_name(d), &root_names);
        root_names.push(rname.clone());
        store::insert_root(
            env.c,
            &RootRow {
                workspace: id.clone(),
                name: rname.clone(),
                ord: i as i64,
                path: fsys::to_str(d),
                ci: fsys::case_insensitive(d),
                added_by: env.caller.label(),
                added_at: env.now,
            },
        )?;
    }
    env.event(
        Some(&id),
        env.caller.session(),
        "workspace.opened",
        json!({ "by": env.caller.label(), "roots": dirs.iter().map(|d| fsys::to_str(d)).collect::<Vec<_>>() }),
    )?;
    env.reload()?;
    let mut outside = Vec::new();
    for (d, rname) in discovered.into_iter().zip(&root_names) {
        store_repos(env, &id, rname, &d.found, true)?;
        outside.extend(d.outside);
    }
    env.reload()?;
    let ws = env.world.ws(&id).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
    let res = OpenResult {
        workspace: view(env, &ws)?,
        added: root_names,
        already_open: false,
        outside_worktrees: outside_view(&outside),
        notes,
    };
    Ok(serde_json::to_value(res).unwrap())
}

fn outside_view(o: &[(PathBuf, Option<String>)]) -> Vec<OutsideWorktree> {
    o.iter().map(|(p, b)| OutsideWorktree { path: fsys::to_str(p), branch: b.clone() }).collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AddArgs {
    paths: Vec<String>,
    workspace: Option<String>,
}

pub fn add(env: &mut Env, req: &Request) -> R<Value> {
    let a: AddArgs = args(req)?;
    if a.paths.is_empty() {
        return Err(Fail::bad("give the directories to add"));
    }
    let ws = env.ws_for(a.workspace.as_deref())?.clone();
    if let Caller::Session(_) = &env.caller {
        if !env.g.cfg.agents.workspace_open {
            return Err(Fail::user_only("Sessions cannot add directories here ([agents] workspace_open = false)."));
        }
        if a.paths.len() != 1 {
            return Err(Fail::user_only("A session can add one checkout at a time."));
        }
    }
    let mut dirs = Vec::new();
    for p in &a.paths {
        let d = canonical(env, p)?;
        if !env.caller.is_user() && env.d.git.toplevel(&d).as_ref() != Some(&d) {
            return Err(Fail::user_only(&format!(
                "{} is not the root of a checkout; a session can add only checkout roots.",
                tilde(&fsys::to_str(&d))
            )));
        }
        if let Some(_already) = check_root(env, &d, Some(&ws.row.id))? {
            continue;
        }
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    let mut names: Vec<String> = ws.roots.iter().map(|r| r.name.clone()).collect();
    let mut added = Vec::new();
    let mut outside = Vec::new();
    let extra_roots: Vec<(String, bool)> = dirs.iter().map(|d| (fsys::to_str(d), fsys::case_insensitive(d))).collect();
    let next_ord = ws.roots.iter().map(|r| r.ord).max().unwrap_or(-1) + 1;
    for (i, d) in dirs.iter().enumerate() {
        let ci = fsys::case_insensitive(d);
        let found = discover(env, d, ci, ws.cfg.repos.scan_depth, &ws.exempt, &extra_roots);
        if !env.caller.is_user() && found.found.len() > env.g.cfg.agents.open_max_checkouts {
            return Err(Fail::user_only(&format!(
                "{} holds {} checkouts; a session may add at most {}.",
                tilde(&fsys::to_str(d)),
                found.found.len(),
                env.g.cfg.agents.open_max_checkouts
            )));
        }
        let rname = unique(&base_name(d), &names);
        names.push(rname.clone());
        store::insert_root(
            env.c,
            &RootRow {
                workspace: ws.row.id.clone(),
                name: rname.clone(),
                ord: next_ord + i as i64,
                path: fsys::to_str(d),
                ci,
                added_by: env.caller.label(),
                added_at: env.now,
            },
        )?;
        env.event(
            Some(&ws.row.id),
            env.caller.session(),
            "workspace.root_added",
            json!({ "root": rname, "path": fsys::to_str(d), "by": env.caller.label() }),
        )?;
        clear_outside_edits(env.c, &ws.row.id, &fsys::to_str(d), ci)?;
        env.reload()?;
        store_repos(env, &ws.row.id, &rname, &found.found, true)?;
        outside.extend(found.outside);
        added.push(rname);
    }
    env.reload()?;
    let ws = env.world.ws(&ws.row.id).cloned().unwrap();
    let res = OpenResult {
        workspace: view(env, &ws)?,
        added,
        already_open: dirs.is_empty(),
        outside_worktrees: outside_view(&outside),
        notes: vec![],
    };
    Ok(serde_json::to_value(res).unwrap())
}

fn clear_outside_edits(c: &rusqlite::Connection, ws_id: &str, root_path: &str, ci: bool) -> R<()> {
    let paths: Vec<String> = {
        let mut st = c.prepare("SELECT DISTINCT path FROM outside_edit WHERE workspace=?1")?;
        let rows = st.query_map([ws_id], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for p in paths {
        if abspath::contains(root_path, &p, ci) {
            c.execute("DELETE FROM outside_edit WHERE workspace=?1 AND path=?2", params![ws_id, p])?;
        }
    }
    Ok(())
}

/// Automatically attaches extra harness workspace directories (`--add-dir`,
/// `/add-dir`, IDE multi-root `workspacePaths`) as persistent roots when the
/// session's primary `cwd` already belongs to an open workspace. Invalid,
/// exempt, nested, or conflicting directories are silently skipped so hooks
/// never fail.
pub fn sync_harness_dirs(env: &mut Env, raw_dirs: &[String]) -> R<Vec<String>> {
    if raw_dirs.is_empty() {
        return Ok(Vec::new());
    }
    let Some(mut ws) = env.world.by_cwd(&env.cwd).cloned() else {
        return Ok(Vec::new());
    };
    let home = user_home().and_then(|h| std::fs::canonicalize(h).ok());
    let mut to_add: Vec<PathBuf> = Vec::new();
    for raw in raw_dirs {
        let p = fsys::absolute(&env.cwd, Path::new(raw));
        let Ok(mut d) = fsys::canonical_dir(&p) else { continue };
        if let Some(top) = env.d.git.toplevel(&d) {
            d = top;
        }
        if d == Path::new("/") || Some(&d) == home.as_ref() {
            continue;
        }
        let s = fsys::to_str(&d);
        if ws.roots.iter().any(|r| abspath::contains(&r.path, &s, r.ci)) {
            continue;
        }
        if exempt::find(&ws.exempt, &s).is_some() {
            continue;
        }
        let Ok(None) = check_root(env, &d, Some(&ws.row.id)) else { continue };
        if to_add.iter().any(|x| x == &d || x.starts_with(&d) || d.starts_with(x)) {
            continue;
        }
        to_add.push(d);
    }
    if to_add.is_empty() {
        return Ok(Vec::new());
    }

    let extra_roots: Vec<(String, bool)> = to_add.iter().map(|d| (fsys::to_str(d), fsys::case_insensitive(d))).collect();
    let mut names: Vec<String> = ws.roots.iter().map(|r| r.name.clone()).collect();
    let mut next_ord = ws.roots.iter().map(|r| r.ord).max().unwrap_or(-1) + 1;
    let mut added = Vec::new();
    for d in to_add {
        let s = fsys::to_str(&d);
        let Ok(None) = check_root(env, &d, Some(&ws.row.id)) else { continue };
        let ci = fsys::case_insensitive(&d);
        let found = discover(env, &d, ci, ws.cfg.repos.scan_depth, &ws.exempt, &extra_roots);
        if found.found.len() > env.g.cfg.agents.open_max_checkouts {
            continue;
        }
        let rname = unique(&base_name(&d), &names);
        names.push(rname.clone());
        store::insert_root(
            env.c,
            &RootRow {
                workspace: ws.row.id.clone(),
                name: rname.clone(),
                ord: next_ord,
                path: s.clone(),
                ci,
                added_by: "user".into(),
                added_at: env.now,
            },
        )?;
        next_ord += 1;
        env.event(
            Some(&ws.row.id),
            env.caller.session(),
            "workspace.root_added",
            json!({ "root": rname, "path": s, "by": "user", "via": "harness" }),
        )?;
        clear_outside_edits(env.c, &ws.row.id, &s, ci)?;
        env.reload()?;
        store_repos(env, &ws.row.id, &rname, &found.found, true)?;
        env.reload()?;
        if let Some(w) = env.world.ws(&ws.row.id).cloned() {
            ws = w;
        }
        added.push(rname);
    }
    Ok(added)
}

pub fn remove_root(env: &Env, ws: &Ws, r: &RootRow, reason: &str) -> R<()> {
    for l in store::leases_ws(env.c, &ws.row.id)? {
        if l.root == r.name {
            engine::release(env, &l, if reason == "gone" { "repo_gone" } else { "workspace_closed" }, None)?;
        }
    }
    env.c.execute("DELETE FROM repo WHERE workspace=?1 AND root=?2", params![ws.row.id, r.name])?;
    env.c.execute("DELETE FROM interest WHERE workspace=?1 AND root=?2", params![ws.row.id, r.name])?;
    env.c.execute("DELETE FROM read_hint WHERE workspace=?1 AND root=?2", params![ws.row.id, r.name])?;
    env.c.execute("DELETE FROM wait_notified WHERE workspace=?1 AND root=?2", params![ws.row.id, r.name])?;
    env.c.execute("DELETE FROM unattributed WHERE workspace=?1 AND root=?2", params![ws.row.id, r.name])?;
    env.c.execute("DELETE FROM root WHERE workspace=?1 AND name=?2", params![ws.row.id, r.name])?;
    remove_hooks(env, &repo_paths(ws, Some(&r.name)))?;
    env.event(
        Some(&ws.row.id),
        env.caller.session(),
        "workspace.root_removed",
        json!({ "root": r.name, "path": r.path, "by": env.caller.label(), "reason": reason }),
    )?;
    Ok(())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RemoveArgs {
    path: String,
    force: bool,
}

pub fn remove(env: &mut Env, req: &Request) -> R<Value> {
    let a: RemoveArgs = args(req)?;
    let d = canonical(env, &a.path)?;
    let s = fsys::to_str(&d);
    let found = env.world.wss.iter().find_map(|w| {
        w.roots.iter().find(|r| abspath::contains(&r.path, &s, r.ci) && abspath::contains(&s, &r.path, r.ci)).map(|r| (w.clone(), r.clone()))
    });
    let Some((ws, r)) = found else {
        return Err(Fail::new(code::NOT_FOUND, format!("{} is not a root of any workspace", tilde(&s))));
    };
    if ws.roots.first().map(|f| f.name.as_str()) == Some(r.name.as_str()) {
        return Err(Fail::refused(format!(
            "{} is the workspace's first root; to stop managing it, close the workspace: tcas workspace close",
            tilde(&s)
        )));
    }
    if a.force {
        env.caller.require_user("tcas workspace remove --force")?;
    }
    if !env.caller.is_user() && r.added_by != env.caller.label() {
        return Err(Fail::user_only(&format!(
            "{} was added by {}; a session can remove only roots it added itself.",
            tilde(&s),
            if r.added_by == "user" { "the user".to_string() } else { format!("session {}", r.added_by) }
        )));
    }
    let n = store::leases_ws(env.c, &ws.row.id)?.iter().filter(|l| l.root == r.name).count();
    if n > 0 && !a.force {
        if env.caller.is_user() {
            return Err(Fail::refused(format!(
                "{} has {n} lease(s) under it. Use --force to drop them.",
                tilde(&s)
            )));
        }
        return Err(Fail::user_only(&format!(
            "{} still has {n} lease(s) under it; a session can remove a root only when it has none.",
            tilde(&s)
        )));
    }
    remove_root(env, &ws, &r, "removed")?;
    Ok(json!({ "workspace": ws.row.name, "removed": r.name, "path": r.path }))
}

pub fn close_ws(env: &Env, ws: &Ws, reason: &str) -> R<usize> {
    let leases = store::leases_ws(env.c, &ws.row.id)?;
    for l in &leases {
        engine::release(env, l, "workspace_closed", None)?;
    }
    for t in ["repo", "root", "interest", "session_ws", "read_hint", "wait_notified", "unattributed"] {
        env.c.execute(&format!("DELETE FROM {t} WHERE workspace=?1"), [&ws.row.id])?;
    }
    env.c.execute("DELETE FROM workspace WHERE id=?1", [&ws.row.id])?;
    remove_hooks(env, &repo_paths(ws, None))?;
    env.event(
        Some(&ws.row.id),
        env.caller.session(),
        "workspace.closed",
        json!({ "by": env.caller.label(), "reason": reason, "name": ws.row.name, "leases_dropped": leases.len() }),
    )?;
    Ok(leases.len())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CloseArgs {
    path: Option<String>,
    workspace: Option<String>,
    force: bool,
}

pub fn close(env: &mut Env, req: &Request) -> R<Value> {
    let a: CloseArgs = args(req)?;
    let spec = a.path.as_deref().or(a.workspace.as_deref());
    let ws = env.ws_for(spec)?.clone();
    if a.force {
        env.caller.require_user("tcas workspace close --force")?;
    }
    let leases = store::leases_ws(env.c, &ws.row.id)?;
    if let Caller::Session(me) = &env.caller {
        if ws.row.opened_by != *me {
            let who = if ws.row.opened_by == "user" { "the user".to_string() } else { format!("session {}", ws.row.opened_by) };
            return Err(Fail::user_only(&format!(
                "Workspace \"{}\" was opened by {who}; a session can close only a workspace it opened itself.",
                ws.row.name
            )));
        }
        if !leases.is_empty() {
            return Err(Fail::user_only(&format!(
                "Workspace \"{}\" still has {} lease(s); a session can close it only when there are none \
                 (commit and release yours first).",
                ws.row.name,
                leases.len()
            )));
        }
        let others: Vec<String> =
            members(env, &ws)?.into_iter().filter(|(id, st)| id != me && st != "ended").map(|(id, _)| id).collect();
        if !others.is_empty() {
            return Err(Fail::user_only(&format!(
                "Other sessions are working in workspace \"{}\" ({}); a session cannot close it under them.",
                ws.row.name,
                others.join(", ")
            )));
        }
    } else if !leases.is_empty() && !a.force {
        return Err(Fail::refused(format!(
            "workspace \"{}\" has {} lease(s). Use --force to drop them.",
            ws.row.name,
            leases.len()
        )));
    }
    let dropped = close_ws(env, &ws, if env.caller.is_user() { "user" } else { "session" })?;
    Ok(json!({ "closed": ws.row.name, "id": ws.row.id, "roots": ws.roots.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), "leases_dropped": dropped }))
}

pub fn list(env: &mut Env) -> R<Value> {
    let wss = env.world.wss.clone();
    let mut out = Vec::new();
    for ws in &wss {
        out.push(view(env, ws)?);
    }
    Ok(serde_json::to_value(out).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SetArgs {
    key: String,
    value: String,
    workspace: Option<String>,
}

pub fn set(env: &mut Env, req: &Request) -> R<Value> {
    let a: SetArgs = args(req)?;
    env.caller.require_user("tcas workspace set")?;
    let ws = env.ws_for(a.workspace.as_deref())?.clone();
    match a.key.as_str() {
        "mode" => {
            let m = Mode::parse(&a.value).ok_or_else(|| Fail::bad("mode is auto or strict"))?;
            env.c.execute("UPDATE workspace SET mode=?2 WHERE id=?1", params![ws.row.id, m.as_str()])?;
            env.event(Some(&ws.row.id), None, "workspace.mode", json!({ "mode": m.as_str() }))?;
            Ok(json!({ "workspace": ws.row.name, "mode": m.as_str() }))
        }
        other => Err(Fail::bad(format!("unknown setting {other:?} (known: mode)"))),
    }
}

/// Root-relative display of an absolute path, for messages.
pub fn rel_display(ws: &Ws, abs: &str) -> Option<String> {
    let r = ws.roots.iter().find(|r| abspath::contains(&r.path, abs, r.ci))?;
    Some(ws.display(&r.name, &relpath::key(&abspath::relative(&r.path, abs), false), false))
}
