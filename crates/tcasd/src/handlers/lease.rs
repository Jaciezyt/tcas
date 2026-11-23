//! `lease.*` (daemon-and-cli.md section 5, "租约").

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use tcas_core::lease::{LeaseState, Source};
use tcas_core::proto::{AcquireResult, LeaseView, Outcome, ReleaseResult, Request, WhoEntry};
use tcas_core::{relpath, wait as wg};

use super::{args, with_tx_inner};
use crate::daemon::{Caller, Daemon, Fail, R};
use crate::engine::{self, AcqReq, Ctx, Env};
use crate::store::{self, LeaseRow};
use crate::waits::{self, WaitEntry};

#[derive(Deserialize, Default)]
#[serde(default)]
struct AcquireArgs {
    paths: Vec<String>,
    brief: Option<String>,
}

fn require_brief(env: &Env, inputs: &[String], brief: &Option<String>) -> R<()> {
    let has = brief.as_deref().is_some_and(|b| !b.trim().is_empty());
    if has {
        return Ok(());
    }
    for i in inputs {
        if let Some(m) = env.resolve(i).managed
            && let Some(ws) = env.world.ws(&m.ws)
            && ws.cfg.brief.require
        {
            return Err(Fail::refused(
                "this workspace requires a brief:  --brief \"<one line: what you are changing and why>\"",
            ));
        }
    }
    Ok(())
}

pub fn acquire(env: &mut Env, req: &Request) -> R<Value> {
    let a: AcquireArgs = args(req)?;
    let sid = env.caller.require_session("tcas lease acquire")?.to_string();
    if a.paths.is_empty() {
        return Err(Fail::bad("give the paths to acquire"));
    }
    require_brief(env, &a.paths, &a.brief)?;
    let acq = engine::acquire(
        env,
        &AcqReq { session: &sid, inputs: &a.paths, source: Source::Cli, brief: a.brief, check_size: true, record_denial: true },
    )?;
    Ok(serde_json::to_value(acq.result).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WaitArgs {
    paths: Vec<String>,
    brief: Option<String>,
    timeout: Option<u64>,
    /// Set when re-registering after tcasd restarted.
    started_at: Option<i64>,
}

fn peer_closed(stream: &UnixStream) -> bool {
    let mut buf = [0u8; 1];
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let closed = match (&*stream).read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    };
    let _ = stream.set_nonblocking(false);
    closed
}

/// `lease.wait`: the response is sent when the wait ends (leases.md section 7).
pub fn wait(d: &'static Daemon, req: &Request, stream: &UnixStream) -> R<Value> {
    let a: WaitArgs = args(req)?;
    let mut list = d.waits();
    let summary = list.summary();
    let (sid, acq, deadline, started_at, cwd, snapshot) = with_tx_inner(d, req, summary, |env| {
        let sid = env.caller.require_session("tcas lease wait")?.to_string();
        if a.paths.is_empty() {
            return Err(Fail::bad("give the paths to wait for"));
        }
        let acq = engine::acquire(
            env,
            &AcqReq {
                session: &sid,
                inputs: &a.paths,
                source: Source::Wait,
                brief: a.brief.clone(),
                check_size: true,
                record_denial: false,
            },
        )?;
        let ws = acq.ws.as_ref().and_then(|w| env.world.ws(w));
        let (def, max) = ws.map(|w| (w.cfg.wait.default_timeout_secs, w.cfg.wait.max_timeout_secs)).unwrap_or((1800, 14400));
        let timeout = a.timeout.unwrap_or(def).min(max) as i64;
        let started_at = a.started_at.filter(|s| *s <= env.now).unwrap_or(env.now);
        if acq.result.outcome == Outcome::Granted && !acq.result.wanted.is_empty() {
            env.event(
                acq.ws.as_deref(),
                Some(&sid),
                "wait.granted",
                json!({ "paths": acq.result.wanted.iter().map(|p| p.display.clone()).collect::<Vec<_>>(), "waited_ms": env.now - started_at }),
            )?;
        }
        let snapshot = if acq.result.outcome == Outcome::Denied {
            acq.result.wanted.iter().filter(|p| !p.is_dir).map(|p| (p.clone(), waits::last_commit(d, &p.abs))).collect()
        } else {
            vec![]
        };
        Ok((sid, acq, started_at + timeout * 1000, started_at, env.cwd.clone(), snapshot))
    })?;
    let now = tcas_sys::clock::now_ms();
    let mut result = acq.result;
    if result.outcome != Outcome::Denied {
        if result.outcome == Outcome::Granted {
            result.waited_ms = Some(now - started_at);
        }
        return Ok(serde_json::to_value(result).unwrap());
    }
    let ws_id = acq.ws.clone().unwrap_or_default();

    // Deadlock at registration: refuse (leases.md section 7).
    let g = waits::graph_with(&list, &sid, &acq.holders);
    if let Some(cycle) = wg::cycle_through(&g, &sid) {
        let mut r = AcquireResult::new(Outcome::Deadlock);
        r.session = sid.clone();
        r.wanted = result.wanted.clone();
        r.blockers = result.blockers.clone();
        r.cycle = waits::describe(&list, &sid, Some((&sid, &acq.holders)), &cycle);
        let conn = d.db();
        store::event(
            &conn,
            now,
            Some(&ws_id),
            Some(&sid),
            "wait.deadlock",
            json!({ "paths": r.wanted.iter().map(|p| p.display.clone()).collect::<Vec<_>>(), "cycle": r.cycle, "at": "registration" }),
        )?;
        return Ok(serde_json::to_value(r).unwrap());
    }
    if now >= deadline {
        result.outcome = Outcome::Timeout;
        result.waited_ms = Some(now - started_at);
        return Ok(serde_json::to_value(result).unwrap());
    }

    let (tx, rx) = mpsc::channel();
    list.next_id += 1;
    let id = list.next_id;
    list.list.push(WaitEntry {
        id,
        session: sid.clone(),
        ws: ws_id.clone(),
        cwd,
        inputs: a.paths.clone(),
        wanted: result.wanted.iter().map(|p| p.display.clone()).collect(),
        brief: a.brief.clone(),
        started_at,
        deadline,
        edges: acq.holders,
        next_change: acq.next_change,
        snapshot,
        tx,
    });
    {
        let conn = d.db();
        store::event(
            &conn,
            now,
            Some(&ws_id),
            Some(&sid),
            if a.started_at.is_some() { "wait.resumed" } else { "wait.started" },
            json!({ "paths": result.wanted.iter().map(|p| p.display.clone()).collect::<Vec<_>>(), "brief": a.brief }),
        )?;
    }
    drop(list);
    d.kick();

    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(r) => return Ok(serde_json::to_value(r).unwrap()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Fail::new("retry", "the wait was dropped; register it again")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if peer_closed(stream) {
                    let mut list = d.waits();
                    if let Some(w) = list.remove(id) {
                        let now = tcas_sys::clock::now_ms();
                        let conn = d.db();
                        let _ = store::event(
                            &conn,
                            now,
                            Some(&w.ws),
                            Some(&w.session),
                            "wait.cancelled",
                            json!({ "paths": w.wanted, "waited_ms": now - w.started_at }),
                        );
                    }
                    return Err(Fail::new("cancelled", "the client went away"));
                }
            }
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReleaseArgs {
    paths: Vec<String>,
    all: bool,
    force: bool,
}

pub fn release(env: &mut Env, req: &Request) -> R<Value> {
    let a: ReleaseArgs = args(req)?;
    if a.force {
        return force_release(env, &a.paths);
    }
    let sid = env.caller.require_session("tcas lease release")?.to_string();
    if a.paths.is_empty() && !a.all {
        return Err(Fail::bad("give the paths to release, or --all"));
    }
    let mut res = ReleaseResult { released: vec![], kept: vec![], released_dirty: vec![], not_held: vec![], unmanaged: vec![] };
    let mine = store::leases_session(env.c, &sid)?;
    let mut by_ws: BTreeMap<String, Vec<&LeaseRow>> = BTreeMap::new();
    for l in &mine {
        by_ws.entry(l.workspace.clone()).or_default().push(l);
    }
    let mut dirty: HashSet<(String, String, String)> = HashSet::new();
    for (wid, ls) in &by_ws {
        if let Some(ws) = env.world.ws(wid) {
            for (r, k) in engine::dirty_of(env, ws, ls) {
                dirty.insert((wid.clone(), r, k));
            }
        }
    }
    let is_dirty = |l: &LeaseRow| dirty.contains(&(l.workspace.clone(), l.root.clone(), l.path_key.clone()));
    let mut gone: HashSet<(String, String, String)> = HashSet::new();
    let drop_lease = |env: &Env, l: &LeaseRow, gone: &mut HashSet<(String, String, String)>| -> R<bool> {
        if gone.insert((l.workspace.clone(), l.root.clone(), l.path_key.clone())) {
            engine::release(env, l, "released", None)?;
            return Ok(true);
        }
        Ok(false)
    };

    if a.all {
        for l in &mine {
            let Some(ws) = env.world.ws(&l.workspace) else { continue };
            let pr = ws.path_ref(&l.root, &l.path, l.is_prefix);
            if !l.is_prefix && is_dirty(l) {
                res.kept.push(pr);
            } else if drop_lease(env, l, &mut gone)? {
                res.released.push(pr);
            }
        }
    }
    for input in &a.paths {
        let r = env.resolve(input);
        let Some(m) = r.managed.clone() else {
            res.unmanaged.extend(r.unmanaged_view());
            continue;
        };
        let ws = env.world.ws(&m.ws).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
        let exact: Vec<&LeaseRow> =
            mine.iter().filter(|l| l.workspace == m.ws && l.root == m.root && l.path_key == m.key).collect();
        if exact.is_empty() {
            if !a.all {
                res.not_held.push(ws.display(&m.root, &m.rel, r.is_dir));
            }
            continue;
        }
        for l in exact {
            let pr = ws.path_ref(&l.root, &l.path, l.is_prefix);
            if l.is_prefix {
                if drop_lease(env, l, &mut gone)? {
                    res.released.push(pr);
                }
                // Own file leases below: clean ones go, dirty ones stay.
                for f in mine.iter().filter(|f| {
                    !f.is_prefix && f.workspace == l.workspace && f.root == l.root && relpath::contains(&l.path_key, &f.path_key)
                }) {
                    let fp = ws.path_ref(&f.root, &f.path, false);
                    if f.repo.is_some() && is_dirty(f) {
                        if !res.kept.contains(&fp) {
                            res.kept.push(fp);
                        }
                    } else if drop_lease(env, f, &mut gone)? {
                        res.released.push(fp);
                    }
                }
            } else {
                let was_dirty = l.repo.is_some() && is_dirty(l);
                if drop_lease(env, l, &mut gone)? {
                    res.kept.retain(|k| k != &pr);
                    if was_dirty { res.released_dirty.push(pr) } else { res.released.push(pr) }
                }
            }
        }
    }
    Ok(serde_json::to_value(res).unwrap())
}

fn force_release(env: &mut Env, paths: &[String]) -> R<Value> {
    env.caller.require_user("tcas lease release --force")?;
    if paths.is_empty() {
        return Err(Fail::bad("give the paths to release"));
    }
    let mut res = ReleaseResult { released: vec![], kept: vec![], released_dirty: vec![], not_held: vec![], unmanaged: vec![] };
    for input in paths {
        let r = env.resolve(input);
        let Some(m) = r.managed.clone() else {
            res.unmanaged.extend(r.unmanaged_view());
            continue;
        };
        let ws = env.world.ws(&m.ws).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
        let found: Vec<LeaseRow> = store::leases_ws(env.c, &m.ws)?
            .into_iter()
            .filter(|l| l.root == m.root && l.path_key == m.key)
            .collect();
        if found.is_empty() {
            res.not_held.push(ws.display(&m.root, &m.rel, r.is_dir));
        }
        for l in found {
            let dirty = !l.is_prefix && engine::dirty_of(env, &ws, &[&l]).contains(&(l.root.clone(), l.path_key.clone()));
            engine::release(env, &l, "forced", Some("user"))?;
            env.notice(
                &l.session,
                "lease.revoked",
                json!({ "workspace": ws.row.id, "path": ws.display(&l.root, &l.path, l.is_prefix), "by": "user" }),
            )?;
            let pr = ws.path_ref(&l.root, &l.path, l.is_prefix);
            if dirty { res.released_dirty.push(pr) } else { res.released.push(pr) }
        }
    }
    Ok(serde_json::to_value(res).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListArgs {
    all: bool,
    workspace: Option<String>,
}

/// Views of leases with their git state; one `git status` per checkout.
pub fn views(env: &Env, leases: &[LeaseRow]) -> R<Vec<LeaseView>> {
    let ctx = Ctx::load(env.c)?;
    let mut by_ws: BTreeMap<String, Vec<&LeaseRow>> = BTreeMap::new();
    for l in leases {
        by_ws.entry(l.workspace.clone()).or_default().push(l);
    }
    let mut out = Vec::new();
    for (wid, ls) in by_ws {
        let Some(ws) = env.world.ws(&wid) else { continue };
        let dirty = engine::dirty_of(env, ws, &ls);
        for l in ls {
            let dv = if l.is_prefix || l.repo.is_none() { None } else { Some(dirty.contains(&(l.root.clone(), l.path_key.clone()))) };
            let mut v = engine::lease_view(env, ws, l, &ctx, dv);
            if l.is_prefix {
                v.files = Some(engine::tracked_under(env, ws, &l.root, &l.path).len());
            }
            out.push(v);
        }
    }
    Ok(out)
}

pub fn list(env: &mut Env, req: &Request) -> R<Value> {
    let a: ListArgs = args(req)?;
    let mut leases = match (&env.caller, a.all) {
        (Caller::Session(s), false) => store::leases_session(env.c, s)?,
        _ => store::leases_all(env.c)?,
    };
    if let Some(spec) = &a.workspace {
        let ws = env.ws_for(Some(spec))?.row.id.clone();
        leases.retain(|l| l.workspace == ws);
    }
    Ok(serde_json::to_value(views(env, &leases)?).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WhoArgs {
    paths: Vec<String>,
}

pub fn who(env: &mut Env, req: &Request) -> R<Value> {
    let a: WhoArgs = args(req)?;
    if a.paths.is_empty() {
        return Err(Fail::bad("give the paths to look up"));
    }
    let mut out = Vec::new();
    for input in &a.paths {
        let r = env.resolve(input);
        let Some(m) = r.managed.clone() else {
            out.push(WhoEntry { input: input.clone(), path: None, unmanaged: r.unmanaged_view(), leases: vec![] });
            continue;
        };
        let ws = env.world.ws(&m.ws).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
        let hits: Vec<LeaseRow> = store::leases_ws(env.c, &m.ws)?
            .into_iter()
            .filter(|l| l.root == m.root && relpath::intersects(&l.path_key, l.is_prefix, &m.key, r.is_dir))
            .collect();
        out.push(WhoEntry {
            input: input.clone(),
            path: Some(ws.path_ref(&m.root, &m.rel, r.is_dir)),
            unmanaged: None,
            leases: views(env, &hits)?,
        });
    }
    Ok(serde_json::to_value(out).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GrantArgs {
    session: String,
    paths: Vec<String>,
    brief: Option<String>,
}

pub fn grant(env: &mut Env, req: &Request) -> R<Value> {
    let a: GrantArgs = args(req)?;
    env.caller.require_user("tcas lease grant")?;
    let s = engine::find_session(env.c, &a.session)?;
    if s.state == "ended" {
        return Err(Fail::refused(format!("session {} has ended", s.id)));
    }
    if a.paths.is_empty() {
        return Err(Fail::bad("give the paths to grant"));
    }
    let brief = a.brief.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
    let acq = engine::acquire(
        env,
        &AcqReq { session: &s.id, inputs: &a.paths, source: Source::Grant, brief: brief.clone(), check_size: false, record_denial: true },
    )?;
    if acq.result.outcome == Outcome::Granted && !acq.result.granted.is_empty() {
        env.notice(
            &s.id,
            "lease.granted_by_user",
            json!({
                "workspace": acq.ws,
                "paths": acq.result.granted.iter().map(|g| g.path.display.clone()).collect::<Vec<_>>(),
                "brief": brief,
            }),
        )?;
    }
    Ok(serde_json::to_value(acq.result).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TransferArgs {
    paths: Vec<String>,
    to: String,
}

pub fn transfer(env: &mut Env, req: &Request) -> R<Value> {
    let a: TransferArgs = args(req)?;
    env.caller.require_user("tcas lease transfer")?;
    let to = engine::find_session(env.c, &a.to)?;
    if to.state == "ended" {
        return Err(Fail::refused(format!("session {} has ended", to.id)));
    }
    let mut moved = Vec::new();
    let mut not_held = Vec::new();
    let mut unmanaged = Vec::new();
    for input in &a.paths {
        let r = env.resolve(input);
        let Some(m) = r.managed.clone() else {
            unmanaged.extend(r.unmanaged_view());
            continue;
        };
        let ws = env.world.ws(&m.ws).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
        let found: Vec<LeaseRow> = store::leases_ws(env.c, &m.ws)?
            .into_iter()
            .filter(|l| l.root == m.root && l.path_key == m.key)
            .collect();
        if found.is_empty() {
            not_held.push(ws.display(&m.root, &m.rel, r.is_dir));
        }
        for l in found {
            if l.session == to.id {
                continue;
            }
            env.c.execute(
                "UPDATE lease SET session=?4, state='active', touched_at=?5 WHERE workspace=?1 AND root=?2 AND path_key=?3",
                params![l.workspace, l.root, l.path_key, to.id, env.now],
            )?;
            let disp = ws.display(&l.root, &l.path, l.is_prefix);
            env.event(
                Some(&l.workspace),
                Some(&to.id),
                "lease.transferred",
                json!({ "root": l.root, "path": l.path, "from": l.session, "to": to.id, "was": if l.state == LeaseState::Orphaned { "orphaned" } else { "active" } }),
            )?;
            env.notice(&l.session, "lease.transferred_away", json!({ "workspace": ws.row.id, "path": disp, "to": to.id }))?;
            env.notice(&to.id, "lease.transferred_to_you", json!({ "workspace": ws.row.id, "path": disp, "from": l.session }))?;
            moved.push(json!({ "path": ws.path_ref(&l.root, &l.path, l.is_prefix), "from": l.session }));
        }
    }
    Ok(json!({ "to": to.id, "moved": moved, "not_held": not_held, "unmanaged": unmanaged }))
}
