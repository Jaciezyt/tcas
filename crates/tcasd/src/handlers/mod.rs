//! Request dispatch. Each request runs in one transaction.

pub mod hook;
pub mod info;
pub mod lease;
pub mod workspace;

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tcas_core::proto::{ErrorBody, Request, Response, VERSION, code};

use crate::daemon::{Daemon, Fail, R, identify};
use crate::engine::{self, Env};
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
                "whoami" => Ok(json!({ "caller": env.caller.label(), "user": env.caller.is_user() })),
                "hook" => hook::handle(env, &req),
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
    with_tx_inner(d, req, waiting, f)
}

/// Runs `f` in one transaction. The caller may hold the waits lock (waits
/// are always locked before the database) and passes their summary.
pub fn with_tx_inner<T>(
    d: &'static Daemon,
    req: &Request,
    waiting: std::collections::HashMap<String, Vec<String>>,
    f: impl FnOnce(&mut Env) -> R<T>,
) -> R<T> {
    let (g, reread) = d.global_checked();
    let mut conn = d.db();
    let tx = conn.transaction()?;
    let now = tcas_sys::clock::now_ms();
    let cwd = canonical_cwd(&req.cwd);
    let caller = identify(&tx, &d.home, req.session.as_deref(), &req.ancestors, &cwd.to_string_lossy(), now)?;
    let world = World::load(&tx)?;
    let mut env = Env { d, c: &tx, world, g, now, caller, cwd, waiting };
    let changed = housekeeping(&mut env)?;
    let v = f(&mut env)?;
    if changed || reread || req.op.starts_with("workspace.") || req.op.starts_with("exempt.") {
        env.reload()?;
        crate::wsjson::write(d, &env.world, &env.g);
    }
    drop(env);
    tx.commit()?;
    drop(conn);
    d.kick();
    Ok(v)
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
    Ok(())
}
