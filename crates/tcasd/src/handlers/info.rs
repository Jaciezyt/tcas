//! `status`, `session.*`, `exempt.*`, `log`, `daemon.*`.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use tcas_core::lease::LeaseState;
use tcas_core::pathfmt;
use tcas_core::proto::{DaemonStatus, ExemptView, Request, SessionView, StatusResult};
use tcas_core::{abspath, exempt, relpath};
use tcas_sys::{fsys, home::user_home};

use super::args;
use super::lease::views;
use super::workspace::{members, view};
use crate::daemon::{Daemon, Fail, R};
use crate::engine::{self, Env};
use crate::store::{self, EventFilter};
use crate::world::Ws;

#[derive(Deserialize, Default)]
#[serde(default)]
struct WsArg {
    workspace: Option<String>,
}

/// Modified paths in a workspace that no lease covers.
fn unattributed(env: &Env, ws: &Ws, leases: &[store::LeaseRow]) -> Vec<tcas_core::proto::PathRef> {
    let mut out = Vec::new();
    for repo in &ws.repos {
        let ci = ws.ci(&repo.root);
        let inner: Vec<String> = ws
            .repos
            .iter()
            .filter(|o| o.root == repo.root && o.dir != repo.dir && relpath::contains(&repo.dir, &o.dir))
            .map(|o| relpath::key(&o.dir, ci))
            .collect();
        for p in env.d.git.changed_paths(&ws.repo_abs(&repo.root, &repo.dir)) {
            let trimmed = p.trim_end_matches('/');
            let rel = relpath::join(&repo.dir, trimmed);
            let key = relpath::key(&rel, ci);
            if inner.iter().any(|i| relpath::contains(i, &key) || relpath::contains(&key, i) && p.ends_with('/')) {
                continue;
            }
            let abs = ws.abs(&repo.root, &rel);
            if exempt::find(&env.g.exempt, &abs).is_some() || exempt::find(&ws.exempt, &abs).is_some() {
                continue;
            }
            let covered = leases.iter().any(|l| {
                l.root == repo.root && tcas_core::lease::covers(&l.path_key, l.is_prefix, &key, p.ends_with('/'))
            });
            if !covered {
                out.push(ws.path_ref(&repo.root, &rel, p.ends_with('/')));
            }
        }
    }
    out
}

pub fn status(env: &mut Env, req: &Request) -> R<Value> {
    let a: WsArg = args(req)?;
    let ws = env.ws_for(a.workspace.as_deref())?.clone();
    // Orphaned leases whose files are clean go now (daemon-and-cli.md section 5).
    let all = store::leases_ws(env.c, &ws.row.id)?;
    let orphans: Vec<&store::LeaseRow> = all.iter().filter(|l| l.state == LeaseState::Orphaned).collect();
    let dirty = engine::dirty_of(env, &ws, &orphans);
    let mut released = Vec::new();
    for l in orphans {
        if l.is_prefix || l.repo.is_none() || !dirty.contains(&(l.root.clone(), l.path_key.clone())) {
            engine::release(env, l, "orphan_clean", None)?;
            released.push(ws.path_ref(&l.root, &l.path, l.is_prefix));
        }
    }
    let leases = store::leases_ws(env.c, &ws.row.id)?;
    let lviews = views(env, &leases)?;
    let mut sessions = Vec::new();
    let ids: BTreeSet<String> = members(env, &ws)?.into_iter().map(|(id, _)| id).collect();
    for id in ids {
        let Some(s) = store::session(env.c, &id)? else { continue };
        sessions.push(session_view(env, &s, &lviews));
    }
    let res = StatusResult {
        workspace: view(env, &ws)?,
        viewer: env.caller.session().map(str::to_string),
        sessions,
        unattributed: unattributed(env, &ws, &leases),
        leases: lviews,
        released,
    };
    Ok(serde_json::to_value(res).unwrap())
}

fn session_view(env: &Env, s: &store::SessionRow, lviews: &[tcas_core::proto::LeaseView]) -> SessionView {
    let mine: Vec<&tcas_core::proto::LeaseView> = lviews.iter().filter(|l| l.session == s.id).collect();
    SessionView {
        id: s.id.clone(),
        harness: s.harness.clone(),
        state: s.state.clone(),
        title: if env.caller.is_user() { s.title.clone() } else { None },
        brief: s.brief.clone(),
        started_at: s.started_at,
        last_seen_at: s.last_seen_at,
        ended_at: s.ended_at,
        leases: mine.len(),
        stalled: mine.iter().filter(|l| l.stalled).count(),
        orphaned: mine.iter().filter(|l| l.state == "orphaned").count(),
        waiting_for: env.waiting.get(&s.id).cloned().unwrap_or_default(),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionListArgs {
    all: bool,
}

pub fn session_list(env: &mut Env, req: &Request) -> R<Value> {
    let a: SessionListArgs = args(req)?;
    let leases = store::leases_all(env.c)?;
    let holders: HashSet<&str> = leases.iter().map(|l| l.session.as_str()).collect();
    let lviews = views(env, &leases)?;
    let mut out = Vec::new();
    for s in store::sessions(env.c)? {
        if !a.all && s.state == "ended" && !holders.contains(s.id.as_str()) {
            continue;
        }
        out.push(session_view(env, &s, &lviews));
    }
    Ok(serde_json::to_value(out).unwrap())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionEndArgs {
    session: String,
}

pub fn session_end(env: &mut Env, req: &Request) -> R<Value> {
    let a: SessionEndArgs = args(req)?;
    env.caller.require_user("tcas session end")?;
    let s = engine::find_session(env.c, &a.session)?;
    let before = store::leases_session(env.c, &s.id)?.len();
    engine::end_session(env, &s.id, "user")?;
    let after = store::leases_session(env.c, &s.id)?;
    env.reload()?;
    let orphaned: Vec<String> = after
        .iter()
        .filter_map(|l| env.world.ws(&l.workspace).map(|w| w.display(&l.root, &l.path, l.is_prefix)))
        .collect();
    Ok(json!({ "session": s.id, "released": before - after.len(), "orphaned": orphaned }))
}

pub fn exempt_list(env: &mut Env) -> R<Value> {
    let mut out: Vec<ExemptView> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for e in env.g.exempt.iter().chain(env.world.wss.iter().flat_map(|w| w.exempt.iter())) {
        if seen.insert((e.written.clone(), e.source.clone())) {
            out.push(ExemptView { path: e.path.clone(), written: e.written.clone(), source: e.source.clone() });
        }
    }
    // Conflicts with roots, for doctor-like warnings.
    let mut conflicts = Vec::new();
    for ws in &env.world.wss {
        for r in &ws.roots {
            if let Some(e) = exempt::find(&env.g.exempt, &r.path) {
                conflicts.push(format!("root {} of workspace \"{}\" is under exempt {}", r.path, ws.row.name, e.written));
            }
        }
    }
    Ok(json!({ "entries": out, "problems": env.g.problems, "conflicts": conflicts }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExemptArgs {
    path: String,
}

fn written_form(env: &Env, input: &str) -> R<(String, std::path::PathBuf)> {
    let home = user_home();
    let abs = if let Some(rest) = input.strip_prefix("~/") {
        home.as_ref().map(|h| h.join(rest)).ok_or_else(|| Fail::bad("HOME is not set"))?
    } else if input == "~" {
        home.clone().ok_or_else(|| Fail::bad("HOME is not set"))?
    } else {
        fsys::absolute(&env.cwd, Path::new(input))
    };
    let abs = std::fs::canonicalize(&abs).unwrap_or_else(|_| fsys::normalize(&abs));
    let written = pathfmt::tilde(&abs, home.as_deref());
    Ok((written, abs))
}

fn edit_config(env: &Env, f: impl FnOnce(&mut toml_edit::Array) -> bool) -> R<bool> {
    let path = env.d.home.config();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut =
        text.parse().map_err(|e| Fail::refused(format!("{} does not parse: {e}", path.display())))?;
    let table = doc
        .entry("exempt")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| Fail::refused("[exempt] in config.toml is not a table"))?;
    let arr = table
        .entry("paths")
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(|| Fail::refused("[exempt] paths in config.toml is not an array"))?;
    let changed = f(arr);
    if changed {
        std::fs::write(&path, doc.to_string()).map_err(|e| Fail::refused(format!("{}: {e}", path.display())))?;
    }
    Ok(changed)
}

pub fn exempt_add(env: &mut Env, req: &Request) -> R<Value> {
    let a: ExemptArgs = args(req)?;
    env.caller.require_user("tcas exempt add")?;
    let (written, abs) = written_form(env, &a.path)?;
    let s = fsys::to_str(&abs);
    for ws in &env.world.wss {
        for r in &ws.roots {
            if abspath::contains(&s, &r.path, r.ci) {
                return Err(Fail::refused(format!(
                    "{written} contains {}, a root of workspace \"{}\". Close the workspace or remove that root first.",
                    r.path, ws.row.name
                )));
            }
        }
    }
    let w2 = written.clone();
    let changed = edit_config(env, |arr| {
        if arr.iter().any(|v| v.as_str() == Some(w2.as_str())) {
            return false;
        }
        arr.push(w2.as_str());
        true
    })?;
    env.d.global();
    env.g = env.d.global();
    env.event(None, None, "exempt.added", json!({ "path": written }))?;
    Ok(json!({ "path": written, "changed": changed }))
}

pub fn exempt_remove(env: &mut Env, req: &Request) -> R<Value> {
    let a: ExemptArgs = args(req)?;
    env.caller.require_user("tcas exempt remove")?;
    let (written, abs) = written_form(env, &a.path)?;
    let raw = a.path.trim_end_matches('/').to_string();
    let abs_s = fsys::to_str(&abs);
    let changed = edit_config(env, |arr| {
        let before = arr.len();
        arr.retain(|v| {
            let Some(s) = v.as_str() else { return true };
            let t = s.trim_end_matches('/');
            !(t == written || t == raw || t == abs_s)
        });
        arr.len() != before
    })?;
    if !changed {
        let builtin = env.g.exempt.iter().any(|e| e.source.starts_with("built-in") && (e.written == written || e.path == abs_s));
        let hint = if builtin { " It is built in; turn the built-in entries off with  [exempt] builtin = false." } else { "" };
        return Err(Fail::refused(format!("{written} is not in [exempt] paths of ~/.tcas/config.toml.{hint}")));
    }
    env.g = env.d.global();
    env.event(None, None, "exempt.removed", json!({ "path": written }))?;
    Ok(json!({ "path": written, "changed": true }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogArgs {
    n: Option<usize>,
    session: Option<String>,
    path: Option<String>,
    after_seq: Option<i64>,
    workspace: Option<String>,
    all: bool,
}

pub fn log(env: &mut Env, req: &Request) -> R<Value> {
    let a: LogArgs = args(req)?;
    let ws = if a.all {
        None
    } else {
        match &a.workspace {
            Some(w) => Some(env.ws_for(Some(w))?.row.id.clone()),
            None => env.world.by_cwd(&env.cwd).map(|w| w.row.id.clone()),
        }
    };
    let session = match &a.session {
        Some(s) => Some(engine::find_session(env.c, s).map(|s| s.id).unwrap_or_else(|_| s.clone())),
        None => None,
    };
    let path_filter = a.path.as_ref().map(|p| {
        let r = env.resolve(p);
        r.managed.map(|m| (m.root, m.rel)).unwrap_or_default()
    });
    let limit = if path_filter.is_some() { 5000 } else { a.n.unwrap_or(50) };
    let mut evs = store::events(
        env.c,
        &EventFilter { workspace: ws.as_deref(), session: session.as_deref(), after_seq: a.after_seq, limit },
    )?;
    if let Some((root, rel)) = path_filter {
        evs.retain(|e| {
            let p = &e.payload;
            let hit_one = p.get("path").and_then(Value::as_str).is_some_and(|x| {
                p.get("root").and_then(Value::as_str).is_none_or(|r| r == root) && (relpath::contains(x, &rel) || relpath::contains(&rel, x))
            });
            let hit_many = p.get("paths").and_then(Value::as_array).is_some_and(|ps| {
                ps.iter().filter_map(Value::as_str).any(|x| x.trim_end_matches('/').ends_with(rel.as_str()))
            });
            hit_one || hit_many
        });
        let n = a.n.unwrap_or(50);
        if evs.len() > n && a.after_seq.is_none() {
            evs.drain(..evs.len() - n);
        }
    }
    Ok(serde_json::to_value(evs).unwrap())
}

pub fn daemon_status(env: &mut Env) -> R<Value> {
    let sessions = store::sessions(env.c)?.iter().filter(|s| s.state != "ended").count();
    let st = DaemonStatus {
        pid: env.d.pid,
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: env.d.started_at,
        home: fsys::to_str(&env.d.home.dir),
        workspaces: env.world.wss.len(),
        sessions,
        waits: env.waiting.values().map(Vec::len).sum(),
    };
    Ok(serde_json::to_value(st).unwrap())
}

pub fn daemon_stop(d: &'static Daemon, req: &Request) -> R<Value> {
    super::with_tx(d, req, |env| {
        env.caller.require_user("tcas daemon stop")?;
        Ok(json!({ "stopping": true, "pid": env.d.pid }))
    })
}
