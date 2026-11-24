//! Request dispatch. Each request runs in one transaction.

pub mod git;
pub mod hook;
pub mod info;
pub mod lease;
pub mod stats;
pub mod workspace;

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use rusqlite::params;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tcas_core::proto::{ErrorBody, Request, Response, VERSION, code};
use tcas_core::relpath;

use crate::daemon::{Daemon, Fail, R, identify};
use crate::engine::{self, Env, Freed};
use crate::store;
use crate::world::World;

pub fn args<T: DeserializeOwned>(req: &Request) -> R<T> {
    let v = if req.args.is_null() { json!({}) } else { req.args.clone() };
    serde_json::from_value(v).map_err(|e| Fail::bad(format!("arguments: {e}")))
}

pub fn dispatch(d: &'static Daemon, req: Request, stream: &UnixStream) -> Response {
    if req.v != VERSION {
        return Response::err(
            code::VERSION_MISMATCH,
            format!("tcas speaks protocol {} but tcasd speaks {VERSION}; install matching versions", req.v),
        );
    }
    let out = match req.op.as_str() {
        "lease.wait" => lease::wait(d, &req, stream),
        "daemon.stop" => info::daemon_stop(d, &req),
        "stats" => stats::handle(d, &req),
        op => {
            let op = op.to_string();
            with_tx(d, &req, |env| match op.as_str() {
                "workspace.open" => workspace::open(env, &req),
                "workspace.add" => workspace::add(env, &req),
                "workspace.remove" => workspace::remove(env, &req),
                "workspace.close" => workspace::close(env, &req),
                "workspace.list" => workspace::list(env),
                "workspace.set" => workspace::set(env, &req),
                "status" => info::status(env, &req),
                "lease.acquire" => lease::acquire(env, &req),
                "lease.release" => lease::release(env, &req),
                "lease.list" => lease::list(env, &req),
                "lease.who" => lease::who(env, &req),
                "lease.grant" => lease::grant(env, &req),
                "lease.transfer" => lease::transfer(env, &req),
                "session.list" => info::session_list(env, &req),
                "session.end" => info::session_end(env, &req),
                "exempt.list" => info::exempt_list(env),
                "exempt.add" => info::exempt_add(env, &req),
                "exempt.remove" => info::exempt_remove(env, &req),
                "log" => info::log(env, &req),
                "daemon.status" => info::daemon_status(env),
                "doctor" => info::doctor(env),
                "whoami" => Ok(json!({ "caller": env.caller.label(), "user": env.caller.is_user() })),
                "hook" => hook::handle(env, &req),
                "git.hook" => git::hook(env, &req),
                "commit.prepare" => git::prepare(env, &req),
                "commit.done" => git::done(env, &req),
                "git_hooks.install" => git::hooks_install(env, &req),
                "git_hooks.remove" => git::hooks_remove(env, &req),
                "git_hooks.status" => git::hooks_status(env, &req),
                other => Err(Fail::bad(format!("unknown operation {other:?}"))),
            })
        }
    };
    match out {
        Ok(v) => Response::ok(v),
        Err(f) => Response {
            ok: false,
            result: None,
            error: Some(ErrorBody { code: f.code, message: f.message, detail: f.detail }),
        },
    }
}

pub fn canonical_cwd(cwd: &str) -> PathBuf {
    let p = PathBuf::from(cwd);
    std::fs::canonicalize(&p).unwrap_or(p)
}

pub fn with_tx(d: &'static Daemon, req: &Request, f: impl FnOnce(&mut Env) -> R<Value>) -> R<Value> {
    let waiting = d.waits().summary();
    let (v, freed) = with_tx_inner_freed(d, req, waiting, f)?;
    notify_freed(d, freed);
    Ok(v)
}

/// Runs `f` in one transaction. The caller may hold the waits lock (waits
/// are always locked before the database) and passes their summary.
pub fn with_tx_inner<T>(
    d: &'static Daemon,
    req: &Request,
    waiting: std::collections::HashMap<String, Vec<String>>,
    f: impl FnOnce(&mut Env) -> R<T>,
) -> R<T> {
    let (v, _) = with_tx_inner_freed(d, req, waiting, f)?;
    Ok(v)
}

pub fn with_tx_inner_freed<T>(
    d: &'static Daemon,
    req: &Request,
    waiting: std::collections::HashMap<String, Vec<String>>,
    f: impl FnOnce(&mut Env) -> R<T>,
) -> R<(T, Vec<Freed>)> {
    let (g, reread) = d.global_checked();
    let mut conn = d.db();
    let tx = conn.transaction()?;
    let now = tcas_sys::clock::now_ms();
    let cwd = canonical_cwd(&req.cwd);
    let caller = identify(&tx, &d.home, req.session.as_deref(), &req.ancestors, &cwd.to_string_lossy(), now)?;
    let world = World::load(&tx)?;
    let mut env = Env { d, c: &tx, world, g, now, caller, agent: None, cwd, waiting, freed: Default::default() };
    let changed = housekeeping(&mut env)?;
    let v = f(&mut env)?;
    if changed || reread || req.op.starts_with("workspace.") || req.op.starts_with("exempt.") {
        env.reload()?;
    }
    crate::wsjson::write(d, &env.world, &env.g);
    let freed = std::mem::take(&mut *env.freed.borrow_mut());
    drop(env);
    tx.commit()?;
    drop(conn);
    d.kick();
    Ok((v, freed))
}

/// Delivers `lease.freed` notices to interested sessions after releasing locks,
/// letting waiting sessions take freed paths synchronously first (harness.md
/// section 6). Never call while holding `d.waits()`.
pub fn notify_freed(d: &'static Daemon, freed: Vec<Freed>) {
    if freed.is_empty() {
        return;
    }
    let has_waits = !d.waits().list.is_empty();
    if has_waits {
        crate::waits::evaluate(d);
    }
    if let Err(e) = notify_freed_db(d, freed) {
        crate::daemon::log(format!("notify_freed: {}", e.message));
    }
}

fn notify_freed_db(d: &'static Daemon, freed: Vec<Freed>) -> R<()> {
    let mut conn = d.db();
    let tx = conn.transaction()?;
    let world = World::load(&tx)?;
    let now = tcas_sys::clock::now_ms();
    let mut notified: HashSet<(String, String, String, String)> = HashSet::new();
    for f in freed {
        let rows: Vec<(String, String, String)> = {
            let mut st = tx.prepare("SELECT session, path, kind FROM interest WHERE workspace=?1 AND root=?2")?;
            st.query_map(params![f.workspace, f.root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        if rows.is_empty() {
            continue;
        }
        let leases: Vec<store::LeaseRow> =
            store::leases_ws(&tx, &f.workspace)?.into_iter().filter(|l| l.root == f.root).collect();
        let ci = world.ws(&f.workspace).map(|w| w.ci(&f.root)).unwrap_or(false);
        for (int_session, int_path, int_kind) in rows {
            let int_is_dir = int_path.ends_with('/');
            let int_rel = if int_path == "." { "" } else { int_path.trim_end_matches('/') };
            let int_key = relpath::key(int_rel, ci);
            if !relpath::intersects(&f.path_key, f.is_prefix, &int_key, int_is_dir) {
                continue;
            }
            let covered_by_self = leases.iter().any(|l| {
                l.session == int_session && tcas_core::lease::covers(&l.path_key, l.is_prefix, &int_key, int_is_dir)
            });
            if int_session == f.releaser
                || int_session == f.holder
                || covered_by_self
                || store::session(&tx, &int_session)?.is_none_or(|s| s.state == "ended")
            {
                tx.execute(
                    "DELETE FROM interest WHERE session=?1 AND workspace=?2 AND root=?3 AND path=?4 AND kind=?5",
                    params![int_session, f.workspace, f.root, int_path, int_kind],
                )?;
                continue;
            }
            let still_held = leases.iter().any(|l| relpath::intersects(&l.path_key, l.is_prefix, &int_key, int_is_dir));
            if still_held {
                continue;
            }
            tx.execute(
                "DELETE FROM interest WHERE session=?1 AND workspace=?2 AND root=?3 AND path=?4 AND kind=?5",
                params![int_session, f.workspace, f.root, int_path, int_kind],
            )?;
            if !notified.insert((int_session.clone(), f.workspace.clone(), f.root.clone(), int_path.clone())) {
                continue;
            }
            let shown = world
                .ws(&f.workspace)
                .map(|w| w.display(&f.root, int_rel, int_is_dir))
                .unwrap_or_else(|| int_path.clone());
            store::notice(
                &tx,
                now,
                &int_session,
                "lease.freed",
                json!({ "workspace": f.workspace, "path": shown, "commit": f.commit }),
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Roots and checkouts whose directories are gone (daemon-and-cli.md section 5).
fn housekeeping(env: &mut Env) -> R<bool> {
    let mut changed = false;
    let wss = env.world.wss.clone();
    for ws in &wss {
        for (i, r) in ws.roots.iter().enumerate() {
            if std::path::Path::new(&r.path).is_dir() {
                continue;
            }
            changed = true;
            if i == 0 {
                workspace::close_ws(env, ws, "gone")?;
                break;
            }
            workspace::remove_root(env, ws, r, "gone")?;
        }
    }
    if changed {
        env.reload()?;
    }
    let wss = env.world.wss.clone();
    for ws in &wss {
        for repo in &ws.repos {
            let dir = ws.repo_abs(&repo.root, &repo.dir);
            if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
                continue;
            }
            changed = true;
            repo_gone(env, ws, &repo.root, &repo.dir)?;
        }
    }
    if changed {
        env.reload()?;
    }
    Ok(changed)
}

pub fn repo_gone(env: &Env, ws: &crate::world::Ws, root: &str, dir: &str) -> R<()> {
    let gone: Vec<(PathBuf, Option<String>)> = ws
        .repos
        .iter()
        .filter(|r| r.root == root && r.dir == dir)
        .map(|r| (ws.repo_abs(root, dir), r.common_dir.clone()))
        .collect();
    env.c.execute(
        "DELETE FROM repo WHERE workspace=?1 AND root=?2 AND dir=?3",
        rusqlite::params![ws.row.id, root, dir],
    )?;
    for l in store::leases_ws(env.c, &ws.row.id)? {
        if l.root == root && l.repo.as_deref() == Some(dir) {
            engine::release(env, &l, "repo_gone", None)?;
        }
    }
    env.event(Some(&ws.row.id), None, "repo.gone", json!({ "root": root, "dir": dir }))?;
    workspace::remove_hooks(env, &gone)
}
