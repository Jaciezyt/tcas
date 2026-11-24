//! Lease operations on the database (leases.md sections 4 to 6). Every
//! function runs inside the caller's transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::json;

use tcas_core::acquire::{self as dec, Held, Want};
use tcas_core::lease::{self, HolderFacts, LeaseFacts, LeaseState, Limits, Source, Why, Yield};
use tcas_core::proto::{AcquireResult, BlockerView, Granted, LeaseView, Outcome, PathRef, TooBig, UnmanagedReason};
use tcas_core::{abspath, brief, relpath};

use crate::daemon::{Caller, Daemon, Fail, Global, R};
use crate::store::{self, LeaseRow, SessionRow};
use crate::world::{self, Managed, Resolved, World, Ws};

pub struct Env<'a> {
    pub d: &'a Daemon,
    pub c: &'a Connection,
    pub world: World,
    pub g: Global,
    pub now: i64,
    pub caller: Caller,
    pub cwd: PathBuf,
    /// Sessions waiting, with the paths they wait for (a snapshot taken
    /// before the database lock; waits are always locked first).
    pub waiting: HashMap<String, Vec<String>>,
}

impl Env<'_> {
    pub fn reload(&mut self) -> R<()> {
        self.world = World::load(self.c)?;
        Ok(())
    }

    pub fn resolve(&self, input: &str) -> Resolved {
        world::resolve(&self.world, &self.g.exempt, &self.d.git, &self.cwd, input)
    }

    pub fn event(&self, ws: Option<&str>, session: Option<&str>, kind: &str, payload: serde_json::Value) -> R<()> {
        store::event(self.c, self.now, ws, session, kind, payload)?;
        Ok(())
    }

    pub fn notice(&self, session: &str, kind: &str, body: serde_json::Value) -> R<()> {
        store::notice(self.c, self.now, session, kind, body)?;
        Ok(())
    }

    /// The workspace for a command: `--workspace`, else the current directory.
    pub fn ws_for(&self, spec: Option<&str>) -> R<&Ws> {
        let found = match spec {
            Some(s) => self.world.find(s, &self.cwd),
            None => self.world.by_cwd(&self.cwd),
        };
        found.ok_or_else(|| match spec {
            Some(s) => Fail::new(tcas_core::proto::code::NOT_FOUND, format!("no workspace matches {s:?}")),
            None => Fail::new(
                tcas_core::proto::code::NOT_IN_WORKSPACE,
                "the current directory is not in any workspace (see tcas workspace list, or pass --workspace)",
            ),
        })
    }
}

pub fn limits(ws: &Ws) -> Limits {
    Limits {
        explicit_idle_ms: ws.cfg.lease.explicit_idle_secs as i64 * 1000,
        waited_grace_ms: ws.cfg.lease.waited_grace_secs as i64 * 1000,
    }
}

pub fn facts(l: &LeaseRow) -> LeaseFacts {
    LeaseFacts {
        is_prefix: l.is_prefix,
        in_repo: l.repo.is_some(),
        state: l.state,
        source: l.source,
        acquired_at: l.acquired_at,
        touched_at: l.touched_at,
        committed_at: l.committed_at,
    }
}

// ---------------------------------------------------------------------------
// Sessions and calls in progress

#[derive(Debug, Clone)]
pub struct Call {
    pub kind: String,
    pub paths: Vec<String>,
    pub background: bool,
    pub opened_at: i64,
}

pub fn calls(c: &Connection) -> R<HashMap<String, Vec<Call>>> {
    let mut st = c.prepare("SELECT session, kind, paths, background_id, opened_at FROM open_call")?;
    let rows = st.query_map([], |r| {
        let paths: Option<String> = r.get(2)?;
        let bg: Option<String> = r.get(3)?;
        Ok((
            r.get::<_, String>(0)?,
            Call {
                kind: r.get(1)?,
                paths: paths.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
                background: bg.is_some(),
                opened_at: r.get(4)?,
            },
        ))
    })?;
    let mut out: HashMap<String, Vec<Call>> = HashMap::new();
    for row in rows {
        let (s, call) = row?;
        out.entry(s).or_default().push(call);
    }
    Ok(out)
}

fn same_path(a: &str, b: &str, ci: bool) -> bool {
    abspath::contains(a, b, ci) && abspath::contains(b, a, ci)
}

/// The holder has an edit call on this lease's path, or a Bash window open
/// (background ones only for their first `window_ms`).
pub fn busy(calls: Option<&Vec<Call>>, lease_abs: &str, is_prefix: bool, ci: bool, now: i64, window_ms: i64) -> bool {
    let Some(calls) = calls else { return false };
    calls.iter().any(|c| match c.kind.as_str() {
        "edit" => c.paths.iter().any(|p| {
            if is_prefix { abspath::contains(lease_abs, p, ci) } else { same_path(p, lease_abs, ci) }
        }),
        "bash" => !c.background || now - c.opened_at < window_ms,
        _ => false,
    })
}

pub fn sessions(c: &Connection) -> R<HashMap<String, SessionRow>> {
    Ok(store::sessions(c)?.into_iter().map(|s| (s.id.clone(), s)).collect())
}

pub struct Ctx {
    pub sessions: HashMap<String, SessionRow>,
    pub calls: HashMap<String, Vec<Call>>,
}

impl Ctx {
    pub fn load(c: &Connection) -> R<Ctx> {
        Ok(Ctx { sessions: sessions(c)?, calls: calls(c)? })
    }

    pub fn holder(&self, ws: &Ws, l: &LeaseRow, now: i64) -> HolderFacts {
        let s = self.sessions.get(&l.session);
        let window = ws.cfg.bash.background_window_secs as i64 * 1000;
        HolderFacts {
            ended: s.is_none_or(|s| s.state == "ended"),
            turn_ended: s.is_some_and(|s| s.state == "idle"),
            busy: busy(self.calls.get(&l.session), &ws.abs(&l.root, &l.path), l.is_prefix, ws.ci(&l.root), now, window),
        }
    }
}

// ---------------------------------------------------------------------------
// git state of leased paths

/// File leases among `leases` whose paths have uncommitted changes, as
/// (root, path_key). One `git status` per checkout.
pub fn dirty_of(env: &Env, ws: &Ws, leases: &[&LeaseRow]) -> HashSet<(String, String)> {
    let mut by_repo: BTreeMap<(String, String), Vec<&LeaseRow>> = BTreeMap::new();
    for l in leases {
        if l.is_prefix {
            continue;
        }
        if let Some(repo) = &l.repo {
            by_repo.entry((l.root.clone(), repo.clone())).or_default().push(l);
        }
    }
    let mut out = HashSet::new();
    for ((root, repo), ls) in by_repo {
        let ci = ws.ci(&root);
        let rels: Vec<String> = ls.iter().map(|l| relpath::strip(repo.as_str(), &l.path)).collect();
        let dirty = env.d.git.dirty(&ws.repo_abs(&root, &repo), &rels, ci);
        for l in ls {
            if dirty.contains(&world::in_repo_key(&l.path, &repo, ci)) {
                out.insert((l.root.clone(), l.path_key.clone()));
            }
        }
    }
    out
}

/// Tracked files at or under `rel_dir`, relative to the root.
pub fn tracked_under(env: &Env, ws: &Ws, root: &str, rel_dir: &str) -> Vec<String> {
    let ci = ws.ci(root);
    let dk = relpath::key(rel_dir, ci);
    let mut out = BTreeSet::new();
    for r in ws.repos.iter().filter(|r| r.root == root) {
        let rk = relpath::key(&r.dir, ci);
        let repo_abs = ws.repo_abs(root, &r.dir);
        if relpath::contains(&rk, &dk) {
            let sub = if rk == dk { String::new() } else { rel_dir[r.dir.len() + usize::from(!r.dir.is_empty())..].to_string() };
            for f in env.d.git.ls_files(&repo_abs, &sub, ci) {
                out.insert(relpath::join(&r.dir, &f));
            }
        } else if relpath::contains(&dk, &rk) {
            for f in env.d.git.ls_files(&repo_abs, "", ci) {
                out.insert(relpath::join(&r.dir, &f));
            }
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Views

pub fn lease_view(env: &Env, ws: &Ws, l: &LeaseRow, ctx: &Ctx, dirty: Option<bool>) -> LeaseView {
    let s = ctx.sessions.get(&l.session);
    let (brief, brief_is_session) = match (&l.brief, s.and_then(|s| s.brief.clone())) {
        (Some(b), _) => (Some(b.clone()), false),
        (None, Some(b)) => (Some(b), true),
        (None, None) => (None, false),
    };
    let holder_state = s.map(|s| s.state.clone()).unwrap_or_else(|| "ended".into());
    let idle_ms = s.map(|s| env.now - s.last_seen_at).unwrap_or(0);
    let window = ws.cfg.bash.background_window_secs as i64 * 1000;
    let has_call = ctx.calls.get(&l.session).is_some_and(|cs| {
        cs.iter().any(|c| c.kind == "edit" || !c.background || env.now - c.opened_at < window)
    });
    let stalled = dirty == Some(true)
        && l.state == LeaseState::Active
        && holder_state != "ended"
        && idle_ms > ws.cfg.lease.stalled_after_secs as i64 * 1000
        && !has_call
        && !env.waiting.contains_key(&l.session);
    let v = lease::verdict(&facts(l), &ctx.holder(ws, l, env.now), env.now, &limits(ws));
    let yields = match (v.yields, v.why) {
        (Yield::Never, _) => "",
        (_, Why::Expired) => "expired",
        (_, Why::Committed) => "committed",
        _ => "",
    };
    LeaseView {
        path: ws.path_ref(&l.root, &l.path, l.is_prefix),
        session: l.session.clone(),
        state: l.state.as_str().into(),
        source: l.source.as_str().into(),
        brief,
        brief_is_session,
        acquired_at: l.acquired_at,
        touched_at: l.touched_at,
        committed_at: l.committed_at,
        dirty,
        stalled,
        holder_state,
        holder_idle_ms: idle_ms,
        holder_title: if env.caller.is_user() { s.and_then(|s| s.title.clone()) } else { None },
        files: None,
        yields: yields.into(),
    }
}

// ---------------------------------------------------------------------------
// Releasing

/// Deletes a lease with its event. `reason` is one of: `commit`, `released`,
/// `yielded`, `expired`, `committed_idle`, `session_ended`, `forced`,
/// `orphan_clean`, `excluded`, `workspace_closed`, `repo_gone`.
pub fn release(env: &Env, l: &LeaseRow, reason: &str, by: Option<&str>) -> R<()> {
    store::delete_lease(env.c, l)?;
    let mut p = json!({ "root": l.root, "path": l.path, "is_prefix": l.is_prefix, "reason": reason, "source": l.source.as_str() });
    if let Some(b) = by {
        p["by"] = json!(b);
    }
    env.event(Some(&l.workspace), Some(&l.session), "lease.released", p)?;
    Ok(())
}

/// Releases any non-prefix lease on `r.abs` when git now excludes it
/// (leases.md section 3).
pub fn release_if_excluded(env: &Env, r: &Resolved) -> R<Vec<PathRef>> {
    if !matches!(r.unmanaged, Some(UnmanagedReason::GitExcluded { .. })) {
        return Ok(vec![]);
    }
    let Some((ws, root)) = env.world.root_of(&r.abs) else { return Ok(vec![]) };
    let key = relpath::key(&abspath::relative(&root.path, &r.abs), root.ci);
    let mut released = Vec::new();
    for l in store::leases_ws(env.c, &ws.row.id)? {
        if l.root == root.name && !l.is_prefix && l.path_key == key {
            release(env, &l, "excluded", None)?;
            released.push(ws.path_ref(&l.root, &l.path, false));
        }
    }
    Ok(released)
}

fn yield_reason(why: Why) -> &'static str {
    match why {
        Why::Implicit | Why::GraceOver => "yielded",
        Why::Expired => "expired",
        Why::Committed => "committed_idle",
        Why::Orphaned | Why::HolderEnded => "orphan_clean",
        _ => "yielded",
    }
}

/// Session end (leases.md section 6, "会话结束").
pub fn end_session(env: &Env, sid: &str, reason: &str) -> R<()> {
    env.c.execute("DELETE FROM open_call WHERE session=?1", [sid])?;
    crate::reconcile::drop_session(env.d, sid);
    let leases = store::leases_session(env.c, sid)?;
    let mut by_ws: BTreeMap<String, Vec<&LeaseRow>> = BTreeMap::new();
    for l in &leases {
        by_ws.entry(l.workspace.clone()).or_default().push(l);
    }
    for (wid, ls) in by_ws {
        let Some(ws) = env.world.ws(&wid) else {
            for l in ls {
                release(env, l, "session_ended", None)?;
            }
            continue;
        };
        let dirty = dirty_of(env, ws, &ls);
        for l in ls {
            if l.is_prefix || l.repo.is_none() || !dirty.contains(&(l.root.clone(), l.path_key.clone())) {
                release(env, l, "session_ended", None)?;
            } else if l.state != LeaseState::Orphaned {
                env.c.execute(
                    "UPDATE lease SET state='orphaned' WHERE workspace=?1 AND root=?2 AND path_key=?3",
                    params![l.workspace, l.root, l.path_key],
                )?;
                env.event(Some(&l.workspace), Some(sid), "lease.orphaned", json!({ "root": l.root, "path": l.path }))?;
            }
        }
    }
    env.c.execute("UPDATE session SET state='ended', ended_at=?2 WHERE id=?1", params![sid, env.now])?;
    env.c.execute("DELETE FROM interest WHERE session=?1", [sid])?;
    env.event(None, Some(sid), "session.ended", json!({ "reason": reason }))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Acquiring

pub struct AcqReq<'a> {
    pub session: &'a str,
    pub inputs: &'a [String],
    pub source: Source,
    pub brief: Option<String>,
    /// Apply `max_dir_files` (not for the user's grants).
    pub check_size: bool,
    /// Write the denial's event and interest (not for re-evaluated waits).
    pub record_denial: bool,
}

pub struct Acq {
    pub result: AcquireResult,
    /// For the wait graph: (holder, wanted path, held path).
    pub holders: Vec<(String, PathRef, PathRef)>,
    pub next_change: Option<i64>,
    pub ws: Option<String>,
}

fn blocker_reason(why: Why, dirty: bool) -> &'static str {
    if dirty {
        return if why == Why::Orphaned { "orphaned" } else { "uncommitted" };
    }
    match why {
        Why::Busy => "busy",
        Why::Grace => "grace",
        _ => "held",
    }
}

/// leases.md section 5. Writes the outcome (grants, yields, events) into the
/// transaction; a denial writes only its event and housekeeping.
pub fn acquire(env: &Env, req: &AcqReq) -> R<Acq> {
    let mut res = AcquireResult::new(Outcome::Granted);
    res.session = req.session.to_string();
    let mut acq = Acq { result: res, holders: vec![], next_change: None, ws: None };

    let mut managed: Vec<(Resolved, Managed)> = Vec::new();
    for input in req.inputs {
        let r = env.resolve(input);
        match r.managed.clone() {
            Some(m) => {
                if !managed.iter().any(|(r2, m2)| m2.ws == m.ws && m2.root == m.root && m2.key == m.key && r2.is_dir == r.is_dir) {
                    managed.push((r, m));
                }
            }
            None => {
                release_if_excluded(env, &r)?;
                acq.result.unmanaged.extend(r.unmanaged_view());
            }
        }
    }
    if managed.is_empty() {
        return Ok(acq);
    }
    let ws_id = managed[0].1.ws.clone();
    if managed.iter().any(|(_, m)| m.ws != ws_id) {
        return Err(Fail::bad("the paths are in different workspaces; request each workspace's paths separately"));
    }
    let ws = env.world.ws(&ws_id).ok_or_else(|| Fail::bad("workspace vanished"))?;
    acq.ws = Some(ws_id.clone());
    acq.result.wanted = managed.iter().map(|(r, m)| ws.path_ref(&m.root, &m.rel, r.is_dir)).collect();

    // A root itself cannot be leased (leases.md section 4).
    for (r, m) in &managed {
        if r.is_dir && m.rel.is_empty() {
            acq.result.outcome = Outcome::Refused;
            acq.result.refusal = Some(format!(
                "{} is a root of the workspace; a lease on it would lock everything. Request narrower directories, or list files.",
                ws.display(&m.root, "", true)
            ));
            return Ok(acq);
        }
    }

    // Directory size.
    let mut dir_files: HashMap<usize, usize> = HashMap::new();
    for (i, (r, m)) in managed.iter().enumerate() {
        if !r.is_dir {
            continue;
        }
        let files = tracked_under(env, ws, &m.root, &m.rel);
        let limit = ws.cfg.lease.max_dir_files;
        if req.check_size && files.len() as u64 > limit {
            let mut children: BTreeMap<String, usize> = BTreeMap::new();
            for f in &files {
                let rest = relpath::strip(&m.rel, f);
                let head = rest.split('/').next().unwrap_or("").to_string();
                let child = if rest.contains('/') {
                    format!("{}/", ws.display(&m.root, &relpath::join(&m.rel, &head), false))
                } else {
                    ws.display(&m.root, &relpath::join(&m.rel, &head), false)
                };
                *children.entry(child).or_default() += 1;
            }
            acq.result.outcome = Outcome::Refused;
            acq.result.too_big = Some(TooBig {
                path: ws.path_ref(&m.root, &m.rel, true),
                files: files.len(),
                limit,
                children: children.into_iter().collect(),
            });
            return Ok(acq);
        }
        dir_files.insert(i, files.len());
    }

    let leases = store::leases_ws(env.c, &ws_id)?;
    let held: Vec<Held> = leases
        .iter()
        .map(|l| Held {
            root: l.root.clone(),
            key: l.path_key.clone(),
            is_prefix: l.is_prefix,
            session: l.session.clone(),
            facts: facts(l),
        })
        .collect();
    let wants: Vec<Want> =
        managed.iter().map(|(r, m)| Want { root: m.root.clone(), key: m.key.clone(), is_dir: r.is_dir }).collect();

    let ctx = Ctx::load(env.c)?;
    let lim = limits(ws);
    let conflicting = dec::conflicting(req.session, &wants, &held);
    let need_git: Vec<&LeaseRow> = conflicting.iter().map(|&i| &leases[i]).collect();
    let dirty = dirty_of(env, ws, &need_git);
    let is_dirty = |i: usize| dirty.contains(&(leases[i].root.clone(), leases[i].path_key.clone()));
    let d = dec::decide(req.session, &wants, &held, |i| ctx.holder(ws, &leases[i], env.now), is_dirty, env.now, &lim);

    for &i in &d.housekeeping {
        release(env, &leases[i], "orphan_clean", None)?;
    }

    if !d.granted {
        acq.result.outcome = Outcome::Denied;
        acq.next_change = d.next_change;
        let mut seen = HashSet::new();
        for b in &d.blockers {
            let l = &leases[b.lease];
            let want = ws.path_ref(&managed[b.want].1.root, &managed[b.want].1.rel, managed[b.want].0.is_dir);
            if seen.insert((b.want, b.lease)) {
                let dirty_v = if l.is_prefix || l.repo.is_none() { None } else { Some(is_dirty(b.lease)) };
                let mut view = lease_view(env, ws, l, &ctx, dirty_v);
                view.holder_title = None;
                acq.result.blockers.push(BlockerView {
                    want: want.clone(),
                    lease: view,
                    reason: blocker_reason(b.why, b.dirty).into(),
                });
            }
            acq.holders.push((l.session.clone(), want, ws.path_ref(&l.root, &l.path, l.is_prefix)));
        }
        if !req.record_denial {
            return Ok(acq);
        }
        for (r, m) in &managed {
            env.c.execute(
                "INSERT OR IGNORE INTO interest(session, workspace, root, path, kind) VALUES(?1,?2,?3,?4,'denied')",
                params![req.session, ws_id, m.root, relpath::display(&m.rel, r.is_dir)],
            )?;
        }
        let holders: BTreeSet<&str> = acq.holders.iter().map(|h| h.0.as_str()).collect();
        env.event(
            Some(&ws_id),
            Some(req.session),
            "lease.denied",
            json!({
                "paths": acq.result.wanted.iter().map(|p| p.display.clone()).collect::<Vec<_>>(),
                "holders": holders,
                "via": req.source.as_str(),
            }),
        )?;
        return Ok(acq);
    }

    let brief = req.brief.as_deref().and_then(|b| brief::sanitize(b, ws.cfg.brief.max_chars));

    // Granted: yields first, then the new leases.
    let mut yielded_to_notify: BTreeMap<String, Vec<(String, &str)>> = BTreeMap::new();
    for &(i, why) in &d.yielded {
        if d.housekeeping.contains(&i) {
            continue;
        }
        let l = &leases[i];
        // A yielded directory leaves the dirty files below it with their
        // holder: they have file leases of their own (leases.md section 6).
        let reason = yield_reason(why);
        release(env, l, reason, Some(req.session))?;
        // The holder is told when it may still count on the lease: one it
        // requested or waited for (harness.md section 6). An implicit lease
        // follows the file and is taken again by the next edit.
        let alive = ctx.sessions.get(&l.session).is_some_and(|s| s.state != "ended");
        if alive && matches!(why, Why::Expired | Why::Committed | Why::GraceOver) {
            yielded_to_notify.entry(l.session.clone()).or_default().push((ws.display(&l.root, &l.path, l.is_prefix), reason));
        }
    }
    let to_brief = brief.clone().or_else(|| ctx.sessions.get(req.session).and_then(|s| s.brief.clone()));
    for (holder, items) in yielded_to_notify {
        let kept: Vec<String> = store::leases_session(env.c, &holder)?
            .iter()
            .filter(|l| l.workspace == ws_id)
            .map(|l| ws.display(&l.root, &l.path, l.is_prefix))
            .collect();
        let (paths, why): (Vec<String>, Vec<&str>) = items.into_iter().unzip();
        env.notice(
            &holder,
            "lease.yielded",
            json!({ "workspace": ws_id, "paths": paths, "why": why, "to": req.session, "to_brief": to_brief, "still_held": kept }),
        )?;
    }
    let mut new_files: Vec<(usize, LeaseRow)> = Vec::new();
    for (wi, (r, m)) in managed.iter().enumerate() {
        let pr = ws.path_ref(&m.root, &m.rel, r.is_dir);
        match d.covered[wi] {
            Some(li) => {
                let l = &leases[li];
                let exact = l.path_key == m.key && l.is_prefix == r.is_dir;
                if l.is_prefix && !r.is_dir && !exact && req.source.is_implicit() {
                    // A file changed under the session's own directory lease
                    // gets its own file lease (leases.md section 4).
                    let row = LeaseRow {
                        workspace: ws_id.clone(),
                        root: m.root.clone(),
                        path: m.rel.clone(),
                        path_key: m.key.clone(),
                        is_prefix: false,
                        repo: m.repo.clone(),
                        session: req.session.to_string(),
                        state: LeaseState::Active,
                        source: req.source,
                        brief: None,
                        acquired_at: env.now,
                        touched_at: env.now,
                        committed_at: None,
                    };
                    store::insert_lease(env.c, &row)?;
                    env.event(
                        Some(&ws_id),
                        Some(req.session),
                        "lease.granted",
                        json!({ "root": m.root, "path": m.rel, "is_prefix": false, "source": req.source.as_str(), "under": l.path }),
                    )?;
                }
                let mut source = l.source;
                if exact && matches!(req.source, Source::Cli | Source::Grant) && !l.source.is_explicit() {
                    source = req.source;
                }
                let new_brief = if exact && brief.is_some() { brief.clone() } else { l.brief.clone() };
                env.c.execute(
                    "UPDATE lease SET touched_at=?4, source=?5, brief=?6 WHERE workspace=?1 AND root=?2 AND path_key=?3",
                    params![l.workspace, l.root, l.path_key, env.now, source.as_str(), new_brief],
                )?;
                acq.result.granted.push(Granted {
                    path: pr,
                    renewed: true,
                    files: dir_files.get(&wi).copied(),
                    foreign_changes: false,
                    changed_in: None,
                });
            }
            None => {
                let row = LeaseRow {
                    workspace: ws_id.clone(),
                    root: m.root.clone(),
                    path: m.rel.clone(),
                    path_key: m.key.clone(),
                    is_prefix: r.is_dir,
                    repo: m.repo.clone(),
                    session: req.session.to_string(),
                    state: LeaseState::Active,
                    source: req.source,
                    brief: brief.clone(),
                    acquired_at: env.now,
                    touched_at: env.now,
                    committed_at: None,
                };
                store::insert_lease(env.c, &row)?;
                env.event(
                    Some(&ws_id),
                    Some(req.session),
                    "lease.granted",
                    json!({ "root": m.root, "path": m.rel, "is_prefix": r.is_dir, "source": req.source.as_str(), "brief": brief }),
                )?;
                acq.result.granted.push(Granted {
                    path: pr,
                    renewed: false,
                    files: dir_files.get(&wi).copied(),
                    foreign_changes: false,
                    changed_in: None,
                });
                if !r.is_dir && m.repo.is_some() {
                    new_files.push((acq.result.granted.len() - 1, row));
                } else if r.is_dir
                    && let Some(repo) = &m.repo
                {
                    // Changes under a new directory lease that are not the
                    // requester's own file leases.
                    let ci = ws.ci(&m.root);
                    let rel = relpath::strip(repo, &m.rel);
                    let rels = if rel.is_empty() { vec![] } else { vec![rel] };
                    let mine: HashSet<String> = store::leases_session(env.c, req.session)?
                        .into_iter()
                        .filter(|l| l.workspace == ws_id && l.root == m.root && !l.is_prefix)
                        .map(|l| l.path_key)
                        .collect();
                    let foreign = env
                        .d
                        .git
                        .dirty(&ws.repo_abs(&m.root, repo), &rels, ci)
                        .into_iter()
                        .map(|k| relpath::key(&relpath::join(repo, &k), ci))
                        .any(|k| !mine.contains(&k));
                    let gi = acq.result.granted.len() - 1;
                    acq.result.granted[gi].foreign_changes = foreign;
                }
            }
        }
    }
    if !new_files.is_empty() {
        let refs: Vec<&LeaseRow> = new_files.iter().map(|(_, l)| l).collect();
        let dirty = dirty_of(env, ws, &refs);
        for (gi, l) in &new_files {
            if dirty.contains(&(l.root.clone(), l.path_key.clone())) {
                acq.result.granted[*gi].foreign_changes = true;
            }
        }
    }
    if let Some(b) = &brief {
        env.c.execute("UPDATE session SET brief=?2 WHERE id=?1", params![req.session, b])?;
    }
    acq.result.brief = brief.clone();
    acq.result.no_brief =
        req.source == Source::Cli && brief.is_none() && acq.result.granted.iter().any(|g| !g.renewed);
    Ok(acq)
}

/// The unique session whose ID is `spec` or starts with it.
pub fn find_session(c: &Connection, spec: &str) -> R<SessionRow> {
    let all = store::sessions(c)?;
    if let Some(s) = all.iter().find(|s| s.id == spec) {
        return Ok(s.clone());
    }
    let m: Vec<&SessionRow> = all.iter().filter(|s| s.id.starts_with(spec) || s.id.split_once(':').is_some_and(|(_, r)| r.starts_with(spec))).collect();
    match m.len() {
        1 => Ok(m[0].clone()),
        0 => Err(Fail::new(tcas_core::proto::code::NOT_FOUND, format!("no session matches {spec:?}"))),
        _ => Err(Fail::bad(format!(
            "{spec:?} matches several sessions: {}",
            m.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

pub fn exists(p: &str) -> bool {
    Path::new(p).exists()
}
