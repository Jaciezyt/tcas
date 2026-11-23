//! The `hook` operation: one harness event, already translated by
//! `tcas hook <harness>` (daemon-and-cli.md section 4, harness.md sections
//! 2 to 7). Nothing here knows which harness sent it.

use std::path::Path;

use rusqlite::params;
use serde_json::{Value, json};

use tcas_core::config::Mode;
use tcas_core::lease::Source;
use tcas_core::proto::{HookRequest, HookResponse, HookTool, Outcome, ProcId, Request};
use tcas_core::{abspath, brief, exempt, lease, relpath};
use tcas_sys::fsys;

use super::args;
use crate::daemon::{self, Caller, Fail, R, Seen};
use crate::engine::{self, AcqReq, Env};
use crate::store;
use crate::text::{self, Viewer};

pub fn handle(env: &mut Env, req: &Request) -> R<Value> {
    let h: HookRequest = args(req)?;
    if h.harness.is_empty() || h.harness.contains(':') || h.session.trim().is_empty() {
        return Err(Fail::bad("hook: the harness and the session are required"));
    }
    let sid = h.session_id();
    let res = if h.event == "session_end" {
        session_end(env, &sid, &h)?;
        HookResponse::default()
    } else {
        let pid = (h.pid > 0).then_some(ProcId { pid: h.pid, started_at: h.pid_started_at });
        let cwd = env.cwd.to_string_lossy().into_owned();
        let seen = Seen {
            cwd: if h.agent.is_none() { Some(&cwd) } else { None },
            pid,
            create: true,
            // A subagent stopping says nothing about the main turn (one was
            // seen firing on a wake-up while idle, probe RESULTS.md).
            activate: h.event != "agent_stop",
            via: "hook",
        };
        daemon::seen(env.c, &env.d.home, &sid, &seen, env.now)?;
        env.caller = Caller::Session(sid.clone());
        match h.event.as_str() {
            "session_start" => session_start(env, &sid, &h)?,
            "prompt" => prompt(env, &sid, &h)?,
            "pre_tool" => pre_tool(env, &sid, &h)?,
            "post_tool" => post_tool(env, &sid, &h)?,
            "stop" => stop(env, &sid)?,
            "agent_stop" => agent_stop(env, &sid, &h)?,
            other => return Err(Fail::bad(format!("hook: unknown event {other:?}"))),
        }
    };
    Ok(serde_json::to_value(res).unwrap())
}

fn with_context(context: Option<String>) -> HookResponse {
    HookResponse { context, ..HookResponse::default() }
}

fn deny(reason: String) -> HookResponse {
    HookResponse { decision: Some("deny".into()), reason: Some(reason), ..HookResponse::default() }
}

// ---------------------------------------------------------------------------
// Session events

fn session_start(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    let mut first = Vec::new();
    if h.source.as_deref() == Some("clear")
        && h.pid > 0
        && let Some(t) = take_over(env, sid, h)?
    {
        first.push(t);
    }
    Ok(with_context(deliver(env, sid, first)?))
}

/// `/clear` gives the conversation a new session ID in the same process
/// (harness.md section 2): the new session continues the old one's leases.
fn take_over(env: &Env, sid: &str, h: &HookRequest) -> R<Option<String>> {
    let olds: Vec<store::SessionRow> = store::sessions(env.c)?
        .into_iter()
        .filter(|s| {
            s.id != sid
                && s.pid == Some(h.pid)
                && s.pid_started_at.is_some_and(|t| (t - h.pid_started_at).abs() <= 1)
        })
        .collect();
    let mut from = Vec::new();
    let mut paths = Vec::new();
    for o in olds {
        let leases = store::leases_session(env.c, &o.id)?;
        for l in &leases {
            env.c.execute(
                "UPDATE lease SET session=?4, state='active' WHERE workspace=?1 AND root=?2 AND path_key=?3",
                params![l.workspace, l.root, l.path_key, sid],
            )?;
            if let Some(ws) = env.world.ws(&l.workspace) {
                paths.push(ws.display(&l.root, &l.path, l.is_prefix));
            }
        }
        env.c.execute(
            "UPDATE notice SET session=?2 WHERE session=?1 AND delivered_at IS NULL",
            params![o.id, sid],
        )?;
        env.c.execute("DELETE FROM open_call WHERE session=?1", [&o.id])?;
        env.c.execute("DELETE FROM interest WHERE session=?1", [&o.id])?;
        if o.state != "ended" {
            env.c.execute("UPDATE session SET state='ended', ended_at=?2 WHERE id=?1", params![o.id, env.now])?;
            env.event(None, Some(&o.id), "session.ended", json!({ "reason": "replaced", "by": sid }))?;
        }
        env.event(None, Some(sid), "session.continued", json!({ "from": o.id, "leases": leases.len() }))?;
        if !leases.is_empty() {
            from.push(o.id.clone());
        }
    }
    Ok(if paths.is_empty() { None } else { Some(text::took_over(&from, &paths)) })
}

fn session_end(env: &mut Env, sid: &str, h: &HookRequest) -> R<()> {
    let Some(s) = store::session(env.c, sid)? else { return Ok(()) };
    if s.state == "ended" {
        return Ok(());
    }
    let reason = h.source.clone().unwrap_or_else(|| "session_end".into());
    if reason == "clear" {
        // A new session in the same process follows at once and takes all
        // the leases over (session_start). Until then nothing is released;
        // dirty files are orphaned as usual, so they read right if it never
        // comes.
        env.c.execute("DELETE FROM open_call WHERE session=?1", [sid])?;
        env.c.execute("DELETE FROM interest WHERE session=?1", [sid])?;
        let leases = store::leases_session(env.c, sid)?;
        for ws in env.world.wss.clone() {
            let ls: Vec<&store::LeaseRow> = leases.iter().filter(|l| l.workspace == ws.row.id).collect();
            let dirty = engine::dirty_of(env, &ws, &ls);
            for l in ls {
                if dirty.contains(&(l.root.clone(), l.path_key.clone())) {
                    env.c.execute(
                        "UPDATE lease SET state='orphaned' WHERE workspace=?1 AND root=?2 AND path_key=?3",
                        params![l.workspace, l.root, l.path_key],
                    )?;
                    env.event(Some(&l.workspace), Some(sid), "lease.orphaned", json!({ "root": l.root, "path": l.path }))?;
                }
            }
        }
        env.c.execute("UPDATE session SET state='ended', ended_at=?2 WHERE id=?1", params![sid, env.now])?;
        env.event(None, Some(sid), "session.ended", json!({ "reason": "clear" }))?;
        return Ok(());
    }
    engine::end_session(env, sid, &reason)
}

fn prompt(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    if let Some(p) = &h.prompt
        && !p.trim_start().starts_with("<task-notification>")
        && let Some(title) = brief::sanitize(p, 80)
    {
        env.c.execute("UPDATE session SET title=?2 WHERE id=?1 AND title IS NULL", params![sid, title])?;
    }
    Ok(with_context(deliver(env, sid, vec![])?))
}

/// The main agent's turn ended (leases.md section 6): its calls are closed
/// and the session is idle until its next hook.
fn stop(env: &mut Env, sid: &str) -> R<HookResponse> {
    env.c.execute("DELETE FROM open_call WHERE session=?1 AND agent IS NULL", [sid])?;
    env.c.execute("UPDATE session SET state='idle' WHERE id=?1 AND state!='ended'", [sid])?;
    Ok(HookResponse::default())
}

fn agent_stop(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    if let Some(a) = &h.agent {
        env.c.execute("DELETE FROM open_call WHERE session=?1 AND agent=?2", params![sid, a])?;
    }
    Ok(HookResponse::default())
}

// ---------------------------------------------------------------------------
// Tool calls

fn pre_tool(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    match &h.tool {
        Some(t) if t.kind == "edit" => pre_edit(env, sid, h, t),
        _ => Ok(HookResponse::default()),
    }
}

/// The paths only the user may change (harness.md section 4 step 1).
fn protected(env: &Env, v: &Viewer, p: &str) -> Option<String> {
    let home = &env.d.home.dir;
    let real = std::fs::canonicalize(home).unwrap_or_else(|_| home.clone());
    for h in [home, &real] {
        if abspath::contains(&fsys::to_str(h), p, false) {
            return Some(text::config_file(&v.tilde(p), "part of tcas's own configuration and data"));
        }
    }
    if let Some((_, root)) = env.world.root_of(p) {
        let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let is = if root.ci { name.eq_ignore_ascii_case(".tcas.toml") } else { name == ".tcas.toml" };
        if is {
            return Some(text::config_file(&v.path(p, false), "tcas's configuration for this workspace"));
        }
    }
    None
}

fn pre_edit(env: &mut Env, sid: &str, h: &HookRequest, t: &HookTool) -> R<HookResponse> {
    let v = Viewer::new(&env.cwd, env.now);
    for p in &t.paths {
        if let Some(reason) = protected(env, &v, p) {
            env.event(None, Some(sid), "edit.refused", json!({ "path": p, "why": "config" }))?;
            return Ok(deny(reason));
        }
    }
    let mut inputs: Vec<String> = Vec::new();
    let mut ws_id = None;
    for p in &t.paths {
        let r = env.resolve(p);
        if let Some(m) = r.managed {
            ws_id.get_or_insert(m.ws);
            inputs.push(r.abs);
        }
    }
    let Some(ws_id) = ws_id else { return Ok(HookResponse::default()) };
    let ws = env.world.ws(&ws_id).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
    let strict = ws.mode(env.g.cfg.defaults.mode) == Mode::Strict;
    let req = AcqReq { session: sid, inputs: &inputs, source: Source::Edit, brief: None, check_size: false, record_denial: true };

    let acq = if strict {
        // Nothing is granted implicitly: try, and undo a grant of anything
        // the session did not already hold.
        env.c.execute_batch("SAVEPOINT strict_edit")?;
        let acq = engine::acquire(env, &req)?;
        let new: Vec<_> = acq.result.granted.iter().filter(|g| !g.renewed).map(|g| g.path.clone()).collect();
        if acq.result.outcome == Outcome::Granted && !new.is_empty() {
            env.c.execute_batch("ROLLBACK TO strict_edit; RELEASE strict_edit")?;
            env.event(Some(&ws_id), Some(sid), "edit.refused", json!({ "paths": inputs, "why": "strict" }))?;
            return Ok(deny(text::strict(&v, &new)));
        }
        env.c.execute_batch("RELEASE strict_edit")?;
        acq
    } else {
        engine::acquire(env, &req)?
    };

    match acq.result.outcome {
        Outcome::Granted => {
            env.c.execute(
                "INSERT OR REPLACE INTO open_call(session, agent, call_id, kind, paths, background_id, opened_at) \
                 VALUES(?1,?2,?3,'edit',?4,NULL,?5)",
                params![sid, h.agent, t.call_id, serde_json::to_string(&inputs).unwrap(), env.now],
            )?;
            let foreign: Vec<String> =
                acq.result.granted.iter().filter(|g| g.foreign_changes).map(|g| g.path.display.clone()).collect();
            if !foreign.is_empty() {
                env.notice(sid, "edit.foreign_changes", json!({ "workspace": ws_id, "paths": foreign }))?;
            }
            Ok(HookResponse::default())
        }
        Outcome::Denied => {
            let c = env.c;
            let d = text::denial(&v, &acq.result, |s| store::session(c, s).ok().flatten().and_then(|s| s.ended_at));
            Ok(HookResponse {
                decision: Some("deny".into()),
                reason: Some(d.reason),
                wait_command: d.wait_command,
                wait_many: d.wait_many,
                needs_user: d.needs_user,
                ..HookResponse::default()
            })
        }
        Outcome::Refused => Ok(deny(format!(
            "tcas: {}",
            acq.result.refusal.unwrap_or_else(|| "this edit is refused.".into())
        ))),
        _ => Ok(HookResponse::default()),
    }
}

fn post_tool(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    let mut extra = Vec::new();
    if let Some(t) = &h.tool {
        env.c.execute("DELETE FROM open_call WHERE session=?1 AND call_id=?2", params![sid, t.call_id])?;
        match t.kind.as_str() {
            "edit" if !t.failed => extra.extend(outside(env, sid, t)?),
            "read" => touch(env, sid, &t.paths)?,
            _ => {}
        }
    }
    Ok(with_context(deliver(env, sid, extra)?))
}

/// An edit outside every workspace, in a git checkout, by a session whose
/// directory is in one (leases.md section 3): recorded, and a hint once per
/// (session, checkout).
fn outside(env: &Env, sid: &str, t: &HookTool) -> R<Vec<String>> {
    let mut out = Vec::new();
    let Some(ws) = env.world.by_cwd(&env.cwd) else { return Ok(out) };
    for p in &t.paths {
        if env.world.root_of(p).is_some() || exempt::find(&env.g.exempt, p).is_some() {
            continue;
        }
        let mut dir = Path::new(p).parent().unwrap_or(Path::new("/")).to_path_buf();
        while !dir.exists() && dir.pop() {}
        let Some(top) = env.d.git.toplevel(&dir) else { continue };
        let top = fsys::to_str(&top);
        if exempt::find(&env.g.exempt, &top).is_some() || env.world.root_of(&top).is_some() {
            continue;
        }
        let known: bool = env
            .c
            .query_row(
                "SELECT COUNT(*) FROM outside_edit WHERE workspace=?1 AND path=?2 AND session=?3",
                params![ws.row.id, top, sid],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;
        env.c.execute(
            "INSERT INTO outside_edit(workspace, path, session, edits, first_at, last_at) VALUES(?1,?2,?3,1,?4,?4) \
             ON CONFLICT(workspace, path, session) DO UPDATE SET edits=edits+1, last_at=?4",
            params![ws.row.id, top, sid, env.now],
        )?;
        if !known {
            env.event(Some(&ws.row.id), Some(sid), "edit.outside", json!({ "path": top }))?;
            if ws.cfg.hint_outside {
                let v = Viewer::new(&env.cwd, env.now);
                out.push(text::outside_hint(&v.path(&top, false)));
            }
        }
    }
    Ok(out)
}

/// Reading or searching a path the session holds counts as using it
/// (leases.md section 6).
fn touch(env: &Env, sid: &str, paths: &[String]) -> R<()> {
    let mine = store::leases_session(env.c, sid)?;
    if mine.is_empty() {
        return Ok(());
    }
    let mut hit = std::collections::HashSet::new();
    for p in paths.iter().take(2000) {
        let Some((ws, root)) = env.world.root_of(p) else { continue };
        let rel = abspath::relative(&root.path, p);
        let key = relpath::key(&rel, root.ci);
        let is_dir = Path::new(p).is_dir();
        for l in &mine {
            if l.workspace != ws.row.id || l.root != root.name {
                continue;
            }
            if lease::covers(&l.path_key, l.is_prefix, &key, is_dir) || is_dir && relpath::contains(&key, &l.path_key) {
                hit.insert((l.workspace.clone(), l.root.clone(), l.path_key.clone()));
            }
        }
    }
    for (w, r, k) in hit {
        env.c.execute(
            "UPDATE lease SET touched_at=?4 WHERE workspace=?1 AND root=?2 AND path_key=?3",
            params![w, r, k, env.now],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Notices

/// Pending notices as one text, yields first (harness.md section 6), then
/// `extra`. Everything returned is marked delivered.
fn deliver(env: &Env, sid: &str, extra: Vec<String>) -> R<Option<String>> {
    let rows: Vec<(i64, String, Value)> = {
        let mut st = env.c.prepare("SELECT id, kind, body FROM notice WHERE session=?1 AND delivered_at IS NULL ORDER BY id")?;
        st.query_map([sid], |r| {
            let body: String = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&body).unwrap_or(Value::Null)))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut parts: Vec<String> = Vec::new();
    let yields: Vec<Value> = rows.iter().filter(|r| r.1 == "lease.yielded").map(|r| r.2.clone()).collect();
    if let Some(y) = text::yielded(&yields) {
        parts.push(y.chars().take(text::MAX_CONTEXT).collect());
    }
    let mut len: usize = parts.iter().map(|p| p.len()).sum();
    let mut more = 0;
    let others = rows.iter().filter(|r| r.1 != "lease.yielded").filter_map(|r| text::notice(&r.1, &r.2));
    for t in others.chain(extra) {
        if len + t.len() + 2 > text::MAX_CONTEXT {
            more += 1;
        } else {
            len += t.len() + 2;
            parts.push(t);
        }
    }
    if more > 0 {
        parts.push(format!("tcas: {more} more notices were not shown."));
    }
    if let Some(last) = rows.last() {
        env.c.execute(
            "UPDATE notice SET delivered_at=?3 WHERE session=?1 AND delivered_at IS NULL AND id<=?2",
            params![sid, last.0, env.now],
        )?;
    }
    Ok(if parts.is_empty() { None } else { Some(parts.join("\n\n")) })
}
