//! `git.hook` (harness.md section 5.6), `commit.prepare` and `commit.done`
//! (daemon-and-cli.md section 5, "提交"), `git_hooks.*`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use tcas_core::lease::{self, LeaseState};
use tcas_core::proto::{CommitDone, CommitPlan, CommitRepo, GitHookAnswer, GitHookArgs, PathRef, Request, code};
use tcas_core::{abspath, relpath};
use tcas_sys::fsys;

use super::args;
use crate::daemon::{Fail, R};
use crate::engine::{self, Env};
use crate::githooks;
use crate::reconcile;
use crate::store::{self, LeaseRow};
use crate::text;
use crate::world::Ws;

/// A checkout known to a workspace.
pub struct Co<'a> {
    pub ws: &'a Ws,
    pub root: String,
    pub dir: String,
    pub abs: PathBuf,
    pub ci: bool,
}

/// The checkout whose root is `abs`.
pub fn checkout_at<'a>(env: &'a Env, abs: &str) -> Option<Co<'a>> {
    let (ws, root) = env.world.root_of(abs)?;
    let rel = abspath::relative(&root.path, abs);
    let k = relpath::key(&rel, root.ci);
    let r = ws.repos.iter().find(|r| r.root == root.name && relpath::key(&r.dir, root.ci) == k)?;
    Some(Co { ws, root: root.name.clone(), dir: r.dir.clone(), abs: ws.repo_abs(&root.name, &r.dir), ci: root.ci })
}

fn canonical(p: &str) -> String {
    std::fs::canonicalize(p).map(|p| fsys::to_str(&p)).unwrap_or_else(|_| p.to_string())
}

/// The holder is gone: its lease is orphaned or its session has ended.
fn left_behind(env: &Env, l: &LeaseRow) -> R<bool> {
    if l.state == LeaseState::Orphaned {
        return Ok(true);
    }
    Ok(store::session(env.c, &l.session)?.is_none_or(|s| s.state == "ended"))
}

/// How a holder is quoted: its session title for the user, its brief for
/// a session (harness.md section 5.6).
fn quote(env: &Env, l: &LeaseRow, for_user: bool) -> R<String> {
    let s = store::session(env.c, &l.session)?;
    let t = if for_user {
        s.as_ref().and_then(|s| s.title.clone())
    } else {
        l.brief.clone().or_else(|| s.as_ref().and_then(|s| s.brief.clone()))
    };
    Ok(t.map(|t| format!(" {}", tcas_core::brief::quoted(&t))).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// git.hook

pub fn hook(env: &mut Env, req: &Request) -> R<Value> {
    let a: GitHookArgs = args(req)?;
    let repo = canonical(&a.repo);
    let Some(co) = checkout_at(env, &repo) else {
        return answer(true, None);
    };
    let ws = co.ws.clone();
    let co = Co { ws: &ws, root: co.root, dir: co.dir, abs: co.abs, ci: co.ci };
    let by = env.caller.session().map(str::to_string);
    let user = by.is_none();
    match a.hook.as_str() {
        "pre-commit" => pre_commit(env, &co, by.as_deref(), &a),
        "post-commit" => {
            let Some(hash) = a.hash.as_deref().filter(|h| !h.is_empty()) else { return answer(true, None) };
            process_commit(env, co.ws, &co.root, &co.dir, hash, &a.paths, by.as_deref())?;
            answer(true, None)
        }
        "pre-rebase" | "stash" => {
            if a.hook == "stash" && !co.ws.cfg.git_hooks.guard_stash || user && !co.ws.cfg.git_hooks.check_user {
                return answer(true, None);
            }
            let others: Vec<LeaseRow> =
                in_checkout(env, &co)?.into_iter().filter(|l| Some(l.session.as_str()) != by.as_deref()).collect();
            if others.is_empty() {
                return answer(true, None);
            }
            let what = if a.hook == "stash" { "git stash" } else { "git rebase" };
            let rows = rows(env, &co, &others, user)?;
            if user && a.allow {
                env.event(Some(&co.ws.row.id), None, "git.override", json!({ "hook": a.hook, "repo": shown(&co), "paths": paths_of(&rows) }))?;
                return answer(true, None);
            }
            env.event(
                Some(&co.ws.row.id),
                by.as_deref(),
                "git.blocked",
                json!({ "hook": a.hook, "repo": shown(&co), "by": by, "paths": paths_of(&rows) }),
            )?;
            answer(false, Some(text::git_refused(what, &shown(&co), &rows, user)))
        }
        "post-checkout" => {
            // A file checkout (flag 0) is left to reconciling.
            if a.args.get(2).map(String::as_str) != Some("1") {
                return answer(true, None);
            }
            let now = env.d.git.branch(&co.abs);
            let last: Option<String> = env
                .c
                .query_row(
                    "SELECT last_branch FROM repo WHERE workspace=?1 AND root=?2 AND dir=?3",
                    params![co.ws.row.id, co.root, co.dir],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            env.event(Some(&co.ws.row.id), by.as_deref(), "git.rewrite", json!({ "hook": a.hook, "repo": shown(&co), "by": by, "from": last, "to": now }))?;
            if now.is_some() && now != last {
                reconcile::branch_changed(env, co.ws, &co.root, &co.dir, last.as_deref(), now.as_deref(), by.as_deref())?;
            }
            answer(true, None)
        }
        "post-merge" | "post-rewrite" => {
            // `commit --amend` also runs post-commit, which handles it.
            if a.hook == "post-rewrite" && a.args.first().map(String::as_str) == Some("amend") {
                return answer(true, None);
            }
            let what = if a.hook == "post-merge" { "merge" } else { "rebase" };
            env.event(Some(&co.ws.row.id), by.as_deref(), "git.rewrite", json!({ "hook": a.hook, "repo": shown(&co), "by": by, "what": what }))?;
            for h in reconcile::holders_in(env, co.ws, &co.root, &co.dir)? {
                if Some(h.as_str()) != by.as_deref() {
                    env.notice(&h, "git.rewrite", json!({ "workspace": co.ws.row.id, "repo": shown(&co), "what": what, "by": by }))?;
                }
            }
            let branch = env.d.git.branch(&co.abs);
            env.c.execute(
                "UPDATE repo SET last_branch=?4 WHERE workspace=?1 AND root=?2 AND dir=?3",
                params![co.ws.row.id, co.root, co.dir, branch],
            )?;
            answer(true, None)
        }
        _ => answer(true, None),
    }
}

fn answer(allow: bool, message: Option<String>) -> R<Value> {
    Ok(serde_json::to_value(GitHookAnswer { allow, message }).unwrap())
}

fn shown(co: &Co) -> String {
    reconcile::repo_shown(co.ws, &co.root, &co.dir)
}

/// Leases in this checkout (directory leases spanning it count).
fn in_checkout(env: &Env, co: &Co) -> R<Vec<LeaseRow>> {
    let dk = relpath::key(&co.dir, co.ci);
    Ok(store::leases_ws(env.c, &co.ws.row.id)?
        .into_iter()
        .filter(|l| l.root == co.root)
        .filter(|l| match &l.repo {
            Some(r) => relpath::key(r, co.ci) == dk,
            None => l.is_prefix && (relpath::contains(&l.path_key, &dk) || relpath::contains(&dk, &l.path_key)),
        })
        .collect())
}

/// (path relative to the checkout, holder, quote, left behind).
type Row = (String, String, String, bool);

fn rows(env: &Env, co: &Co, ls: &[LeaseRow], for_user: bool) -> R<Vec<Row>> {
    let mut out = Vec::new();
    for l in ls {
        let rel = if relpath::contains(&relpath::key(&co.dir, co.ci), &l.path_key) {
            relpath::display(&relpath::strip(&co.dir, &l.path), l.is_prefix)
        } else {
            co.ws.display(&l.root, &l.path, l.is_prefix)
        };
        out.push((rel, l.session.clone(), quote(env, l, for_user)?, left_behind(env, l)?));
    }
    Ok(out)
}

fn paths_of(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|r| r.0.clone()).collect()
}

fn pre_commit(env: &Env, co: &Co, by: Option<&str>, a: &GitHookArgs) -> R<Value> {
    let user = by.is_none();
    if user && !co.ws.cfg.git_hooks.check_user {
        return answer(true, None);
    }
    let leases = in_checkout(env, co)?;
    let mut hit: Vec<(String, &LeaseRow)> = Vec::new();
    for p in &a.paths {
        let k = relpath::key(&relpath::join(&co.dir, p), co.ci);
        if let Some(l) = leases.iter().find(|l| Some(l.session.as_str()) != by && lease::covers(&l.path_key, l.is_prefix, &k, false)) {
            hit.push((p.clone(), l));
        }
    }
    if hit.is_empty() {
        return answer(true, None);
    }
    let mut live: Vec<Row> = Vec::new();
    let mut gone: Vec<Row> = Vec::new();
    for (p, l) in &hit {
        let row = (p.clone(), l.session.clone(), quote(env, l, user)?, left_behind(env, l)?);
        if row.3 { gone.push(row) } else { live.push(row) }
    }
    let paths: Vec<String> = hit.iter().map(|h| h.0.clone()).collect();
    if user && live.is_empty() {
        return answer(true, Some(text::commit_left_behind(&gone)));
    }
    if user && a.allow {
        env.event(Some(&co.ws.row.id), None, "git.override", json!({ "hook": "pre-commit", "repo": shown(co), "paths": paths }))?;
        return answer(true, None);
    }
    env.event(
        Some(&co.ws.row.id),
        by,
        "git.blocked",
        json!({ "hook": "pre-commit", "repo": shown(co), "by": by, "paths": paths }),
    )?;
    let mut all = live;
    if !user {
        all.extend(gone);
    }
    answer(false, Some(text::commit_refused(&all, user)))
}

// ---------------------------------------------------------------------------
// Commits

/// A commit in a checkout (harness.md section 5.4, leases.md section 6):
/// the committer's implicit and waited-for leases on paths now clean are
/// released, its explicit ones remember the commit, and leases left behind
/// on paths now clean are released. Each commit is processed once.
pub fn process_commit(env: &Env, ws: &Ws, root: &str, dir: &str, hash: &str, paths: &[String], by: Option<&str>) -> R<CommitDone> {
    let seen: Option<String> =
        // The same hash can be made in two checkouts (identical trees,
        // parents and times), so a commit counts once per checkout.
        env.c
            .query_row(
                "SELECT result FROM commit_seen WHERE workspace=?1 AND root=?2 AND repo=?3 AND hash=?4",
                params![ws.row.id, root, dir, hash],
                |r| r.get(0),
            )
            .optional()?;
    if let Some(r) = seen {
        let mut done: CommitDone = serde_json::from_str(&r).unwrap_or_default();
        done.still_held = still_held(env, ws, by)?;
        return Ok(done);
    }
    let ci = ws.ci(root);
    let repo_abs = ws.repo_abs(root, dir);
    let dirty = if paths.is_empty() { Default::default() } else { env.d.git.dirty(&repo_abs, paths, ci) };
    let clean: BTreeSet<String> = paths
        .iter()
        .filter(|p| !dirty.contains(&relpath::key(p, ci)))
        .map(|p| relpath::key(&relpath::join(dir, p), ci))
        .collect();
    let all_keys: Vec<String> = paths.iter().map(|p| relpath::key(&relpath::join(dir, p), ci)).collect();

    let mut released: Vec<PathRef> = Vec::new();
    for l in store::leases_ws(env.c, &ws.row.id)? {
        if l.root != root {
            continue;
        }
        let mine = Some(l.session.as_str()) == by;
        if l.is_prefix {
            if mine && all_keys.iter().any(|k| relpath::contains(&l.path_key, k)) {
                committed(env, &l)?;
            }
            continue;
        }
        if !all_keys.contains(&l.path_key) {
            continue;
        }
        let is_clean = clean.contains(&l.path_key);
        if mine {
            if is_clean && (l.source.is_implicit() || l.source == tcas_core::lease::Source::Wait) {
                engine::release_with_commit(env, &l, "commit", by, Some(hash))?;
                released.push(ws.path_ref(&l.root, &l.path, false));
            } else {
                committed(env, &l)?;
            }
        } else if is_clean && left_behind(env, &l)? {
            engine::release_with_commit(env, &l, "orphan_clean", by, Some(hash))?;
            released.push(ws.path_ref(&l.root, &l.path, false));
        }
    }
    for k in &all_keys {
        env.c.execute(
            "DELETE FROM unattributed WHERE workspace=?1 AND root=?2 AND path=?3",
            params![ws.row.id, root, k],
        )?;
    }
    let shown: Vec<String> = paths.iter().map(|p| ws.display(root, &relpath::join(dir, p), false)).collect();
    env.event(
        Some(&ws.row.id),
        by,
        "commit",
        json!({ "hash": hash, "repo": reconcile::repo_shown(ws, root, dir), "paths": shown, "by": by, "released": released.len() }),
    )?;
    let done = CommitDone { released, still_held: vec![] };
    env.c.execute(
        "INSERT OR REPLACE INTO commit_seen(hash, workspace, root, repo, by, at, result) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![hash, ws.row.id, root, dir, by, env.now, serde_json::to_string(&done).unwrap()],
    )?;
    Ok(CommitDone { still_held: still_held(env, ws, by)?, ..done })
}

fn committed(env: &Env, l: &LeaseRow) -> R<()> {
    env.c.execute(
        "UPDATE lease SET committed_at=?4, touched_at=?4 WHERE workspace=?1 AND root=?2 AND path_key=?3",
        params![l.workspace, l.root, l.path_key, env.now],
    )?;
    Ok(())
}

fn still_held(env: &Env, ws: &Ws, by: Option<&str>) -> R<Vec<PathRef>> {
    let Some(s) = by else { return Ok(vec![]) };
    Ok(store::leases_session(env.c, s)?
        .into_iter()
        .filter(|l| l.workspace == ws.row.id)
        .map(|l| ws.path_ref(&l.root, &l.path, l.is_prefix))
        .collect())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PrepareArgs {
    paths: Vec<String>,
}

/// `commit.prepare`: the session's changed files, grouped by checkout.
pub fn prepare(env: &mut Env, req: &Request) -> R<Value> {
    let sid = env.caller.require_session("tcas commit")?.to_string();
    let a: PrepareArgs = args(req)?;
    let mine = store::leases_session(env.c, &sid)?;
    // (workspace, root, root-relative path, is the path itself asked for)
    let mut want: BTreeMap<(String, String, String), bool> = BTreeMap::new();
    let mut outside_repo: Vec<String> = Vec::new();
    if a.paths.is_empty() {
        for l in mine.iter().filter(|l| !l.is_prefix) {
            want.insert((l.workspace.clone(), l.root.clone(), l.path.clone()), false);
        }
    } else {
        let mut refused: Vec<String> = Vec::new();
        for input in &a.paths {
            let r = env.resolve(input);
            let Some(m) = r.managed.clone() else {
                refused.push(format!("{input} (not managed by tcas)"));
                continue;
            };
            let under: Vec<&LeaseRow> = mine
                .iter()
                .filter(|l| l.workspace == m.ws && l.root == m.root && !l.is_prefix && relpath::contains(&m.key, &l.path_key))
                .collect();
            let covered = mine.iter().any(|l| l.workspace == m.ws && l.root == m.root && lease::covers(&l.path_key, l.is_prefix, &m.key, r.is_dir));
            if r.is_dir {
                if under.is_empty() && !covered {
                    refused.push(relpath::display(&m.rel, true));
                }
                for l in under {
                    want.entry((l.workspace.clone(), l.root.clone(), l.path.clone())).or_insert(false);
                }
                // A submodule named itself: its recorded commit in the outer checkout.
                if covered && let Some(ws) = env.world.ws(&m.ws) && ws.repos.iter().any(|x| x.root == m.root && x.dir == m.rel && x.kind == "submodule") {
                    want.insert((m.ws.clone(), m.root.clone(), m.rel.clone()), true);
                }
            } else if covered {
                want.insert((m.ws.clone(), m.root.clone(), m.rel.clone()), true);
            } else {
                refused.push(r.input.clone());
            }
        }
        if !refused.is_empty() {
            return Err(Fail::new(
                code::LEASE_DENIED,
                format!(
                    "not under this session's leases: {}. tcas commit only commits files this session holds; request \
                     them first (tcas lease acquire …) or leave them out.",
                    refused.join(", ")
                ),
            ));
        }
    }

    let mut plan = CommitPlan::default();
    // (workspace, root, checkout dir) -> paths relative to the root, asked
    let mut groups: BTreeMap<(String, String, String), Vec<(String, bool)>> = BTreeMap::new();
    for ((wid, root, rel), asked) in want {
        let Some(ws) = env.world.ws(&wid) else { continue };
        let abs = ws.abs(&root, &rel);
        // The owning checkout, from the parent: a submodule's own path
        // belongs to the outer checkout.
        let _ = asked;
        let probe = Path::new(&abs).parent().map(fsys::to_str).unwrap_or(abs.clone());
        let root_path = ws.root(&root).map(|r| r.path.clone()).unwrap_or_default();
        let mut start = PathBuf::from(&probe);
        while !start.exists() && start.pop() {}
        let repo = if abspath::contains(&root_path, &fsys::to_str(&start), ws.ci(&root)) {
            crate::repos::owning(&root_path, &fsys::to_str(&start))
        } else {
            None
        };
        match repo {
            Some(d) => groups.entry((wid.clone(), root.clone(), d)).or_default().push((rel, asked)),
            None => outside_repo.push(ws.display(&root, &rel, false)),
        }
    }
    for ((wid, root, dir), items) in groups {
        let Some(ws) = env.world.ws(&wid) else { continue };
        let ci = ws.ci(&root);
        let abs = ws.repo_abs(&root, &dir);
        let rels: Vec<String> = items.iter().map(|(p, _)| relpath::strip(&dir, p)).collect();
        let dirty = env.d.git.dirty(&abs, &rels, ci);
        let mut paths = Vec::new();
        let mut shown = Vec::new();
        for ((p, asked), r) in items.iter().zip(&rels) {
            if dirty.contains(&relpath::key(r, ci)) {
                paths.push(r.clone());
                shown.push(ws.display(&root, p, false));
            } else if *asked {
                plan.clean.push(ws.display(&root, p, false));
            }
        }
        if paths.is_empty() {
            continue;
        }
        plan.repos.push(CommitRepo {
            display: reconcile::repo_shown(ws, &root, &dir),
            abs: fsys::to_str(&abs),
            paths,
            shown,
            hooks: githooks::installed(&env.d.git, &env.d.home, &abs),
        });
    }
    plan.repos.sort_by(|a, b| a.display.cmp(&b.display));
    plan.outside_repo = outside_repo;
    Ok(serde_json::to_value(plan).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DoneArgs {
    repo: String,
    hash: String,
    paths: Vec<String>,
}

/// `commit.done`: `tcas commit` reports a commit it made (the post-commit
/// hook usually did already; each commit is processed once).
pub fn done(env: &mut Env, req: &Request) -> R<Value> {
    let a: DoneArgs = args(req)?;
    let repo = canonical(&a.repo);
    let Some(co) = checkout_at(env, &repo) else {
        return Err(Fail::new(code::NOT_FOUND, format!("{repo} is not a checkout in any workspace")));
    };
    let ws = co.ws.clone();
    let by = env.caller.session().map(str::to_string);
    let d = process_commit(env, &ws, &co.root, &co.dir, &a.hash, &a.paths, by.as_deref())?;
    Ok(serde_json::to_value(d).unwrap())
}

// ---------------------------------------------------------------------------
// git_hooks.*

#[derive(Deserialize, Default)]
#[serde(default)]
struct HooksArgs {
    repo: Option<String>,
}

fn target_repos(env: &Env, a: &HooksArgs) -> R<Vec<PathBuf>> {
    if let Some(r) = &a.repo {
        let p = fsys::absolute(&env.cwd, Path::new(r));
        let top = env.d.git.toplevel(&p).ok_or_else(|| Fail::bad(format!("{r} is not in a git checkout")))?;
        return Ok(vec![top]);
    }
    if let Some(top) = env.d.git.toplevel(&env.cwd) {
        return Ok(vec![top]);
    }
    let ws = env.ws_for(None)?;
    Ok(ws.repos.iter().map(|r| ws.repo_abs(&r.root, &r.dir)).collect())
}

pub fn hooks_install(env: &mut Env, req: &Request) -> R<Value> {
    env.caller.require_user("tcas git-hooks install")?;
    if !githooks::supported(&env.d.git) {
        return Err(Fail::refused("this git does not run hooks defined in its configuration; tcas's git hooks cannot be installed"));
    }
    let a: HooksArgs = args(req)?;
    let mut out = Vec::new();
    for r in target_repos(env, &a)? {
        let changed = githooks::install(&env.d.git, &env.d.home, &r).map_err(Fail::refused)?;
        if changed {
            env.event(None, None, "git_hooks.installed", json!({ "repo": fsys::to_str(&r) }))?;
        }
        out.push(json!({ "repo": fsys::to_str(&r), "installed": true, "changed": changed }));
    }
    Ok(json!({ "repos": out }))
}

pub fn hooks_remove(env: &mut Env, req: &Request) -> R<Value> {
    env.caller.require_user("tcas git-hooks remove")?;
    let a: HooksArgs = args(req)?;
    let mut out = Vec::new();
    for r in target_repos(env, &a)? {
        let changed = githooks::remove(&env.d.git, &env.d.home, &r).map_err(Fail::refused)?;
        if changed {
            env.event(None, None, "git_hooks.removed", json!({ "repo": fsys::to_str(&r) }))?;
        }
        out.push(json!({ "repo": fsys::to_str(&r), "installed": false, "changed": changed }));
    }
    Ok(json!({ "repos": out }))
}

pub fn hooks_status(env: &mut Env, req: &Request) -> R<Value> {
    let a: HooksArgs = args(req)?;
    let supported = githooks::supported(&env.d.git);
    let mut out = Vec::new();
    for r in target_repos(env, &a)? {
        out.push(json!({ "repo": fsys::to_str(&r), "installed": githooks::installed(&env.d.git, &env.d.home, &r) }));
    }
    Ok(json!({ "supported": supported, "config": fsys::to_str(&env.d.home.git_hooks_config()), "repos": out }))
}
