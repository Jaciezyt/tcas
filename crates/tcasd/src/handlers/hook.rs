//! The `hook` operation: one harness event, already translated by
//! `tcas hook <harness>` (daemon-and-cli.md section 4, harness.md sections
//! 2 to 7). Nothing here knows which harness sent it.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::params;
use serde_json::{Value, json};

use tcas_core::config::Mode;
use tcas_core::lease::{LeaseState, Source};
use tcas_core::proto::{HookRequest, HookResponse, HookTool, Outcome, ProcId, Request};
use tcas_core::{abspath, brief, exempt, guard, lease, relpath, shell};
use tcas_sys::fsys;

use super::args;
use crate::daemon::{self, Caller, Fail, R, Seen};
use crate::engine::{self, AcqReq, Env};
use crate::store::LeaseRow;
use crate::text::{self, OtherSessionIntro, Viewer};
use crate::{bashguard, reconcile, store};

pub fn handle(env: &mut Env, req: &Request) -> R<Value> {
    let mut h: HookRequest = args(req)?;
    if h.harness.is_empty() || h.harness.contains(':') || h.session.trim().is_empty() {
        return Err(Fail::bad("hook: the harness and the session are required"));
    }
    let raw_sid = h.session_id();
    let sid = resolve_subagent(env, &mut h, &raw_sid)?;
    env.agent = h.agent.clone();
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
        if h.agent.is_none() {
            super::workspace::sync_harness_dirs(env, &h.workspace_dirs)?;
        }
        match h.event.as_str() {
            "session_start" => session_start(env, &sid, &h)?,
            "prompt" => prompt(env, &sid, &h)?,
            "dir_added" => HookResponse::default(),
            "pre_tool" => pre_tool(env, &sid, &h)?,
            "post_tool" => post_tool(env, &sid, &h)?,
            "post_invocation" => post_invocation(env, &sid)?,
            "stop" => stop(env, &sid, &h)?,
            "agent_stop" => agent_stop(env, &sid, &h)?,
            other => return Err(Fail::bad(format!("hook: unknown event {other:?}"))),
        }
    };
    Ok(serde_json::to_value(res).unwrap())
}

/// Maps a harness subagent conversation (e.g. AGY `invoke_subagent`, which gets
/// its own `conversationId` inside the parent `agy` process with empty user
/// input) onto its live parent session so both share leases (leases.md §9).
fn resolve_subagent(env: &Env, h: &mut HookRequest, raw_sid: &str) -> R<String> {
    if !h.subagent_candidate {
        let _ = env.c.execute("DELETE FROM session_alias WHERE alias=?1", [raw_sid]);
        return Ok(raw_sid.to_string());
    }
    let existing: Option<String> = env
        .c
        .query_row("SELECT parent FROM session_alias WHERE alias=?1", [raw_sid], |r| r.get(0))
        .ok();
    if let Some(parent) = existing
        && store::session(env.c, &parent)?.is_some_and(|s| s.state != "ended")
    {
        h.agent = Some(h.session.clone());
        if h.event == "stop" {
            h.event = "agent_stop".into();
        }
        return Ok(parent);
    }
    // If this ID is already a known root session, keep it.
    if store::session(env.c, raw_sid)?.is_some() {
        return Ok(raw_sid.to_string());
    }
    if h.pid > 0 {
        let mut candidates: Vec<store::SessionRow> = store::sessions(env.c)?
            .into_iter()
            .filter(|s| {
                s.id != raw_sid
                    && s.harness == h.harness
                    && s.state != "ended"
                    && s.pid == Some(h.pid)
                    && s.pid_started_at.is_some_and(|t| (t - h.pid_started_at).abs() <= 1)
            })
            .collect();
        candidates.sort_by_key(|s| -s.started_at);
        if let Some(parent) = candidates.first() {
            env.c.execute(
                "INSERT OR REPLACE INTO session_alias(alias, parent, created_at) VALUES(?1,?2,?3)",
                params![raw_sid, parent.id, env.now],
            )?;
            daemon::mark(&env.d.home, raw_sid);
            h.agent = Some(h.session.clone());
            if h.event == "stop" {
                h.event = "agent_stop".into();
            }
            return Ok(parent.id.clone());
        }
    }
    Ok(raw_sid.to_string())
}

fn with_context(context: Option<String>) -> HookResponse {
    HookResponse { context, ..HookResponse::default() }
}

fn deny(reason: String) -> HookResponse {
    HookResponse { decision: Some("deny".into()), reason: Some(reason), ..HookResponse::default() }
}

// ---------------------------------------------------------------------------
// Session events

fn session_workspaces(env: &Env, sid: &str, extra_paths: &[String]) -> R<Vec<String>> {
    let mut ids = Vec::new();
    let mut push = |id: String| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    if let Some(ws) = env.world.by_cwd(&env.cwd) {
        push(ws.row.id.clone());
    }
    for p in extra_paths {
        if let Some((ws, _)) = env.world.root_of(p) {
            push(ws.row.id.clone());
        }
    }
    for l in store::leases_session(env.c, sid)? {
        push(l.workspace);
    }
    Ok(ids)
}

fn has_session_ws(env: &Env, sid: &str, ws_id: &str) -> R<bool> {
    let n: i64 = env.c.query_row(
        "SELECT COUNT(*) FROM session_ws WHERE session=?1 AND workspace=?2",
        params![sid, ws_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn format_lease_item(ws: &crate::world::Ws, l: &LeaseRow) -> String {
    let disp = ws.display(&l.root, &l.path, l.is_prefix);
    let mut s = if l.is_prefix { format!("{disp} (directory)") } else { disp };
    if l.state == LeaseState::Orphaned {
        s.push_str(" (orphaned)");
    }
    s
}

fn intro_if_needed(env: &Env, sid: &str, ws_ids: &[String], out: &mut Vec<String>) -> R<()> {
    if ws_ids.is_empty() {
        return Ok(());
    }
    let session = store::session(env.c, sid)?;
    let sessions: BTreeMap<String, store::SessionRow> =
        store::sessions(env.c)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let v = Viewer::new(&env.cwd, env.now);
    let cwd_str = fsys::to_str(&env.cwd);
    for ws_id in ws_ids {
        if has_session_ws(env, sid, ws_id)? {
            continue;
        }
        let Some(ws) = env.world.ws(ws_id) else { continue };
        let mode = ws.mode(env.g.cfg.defaults.mode);
        let just_opened = session.as_ref().is_some_and(|s| ws.row.created_at > s.started_at);
        let cur_root = ws.roots.iter().find(|r| abspath::contains(&r.path, &cwd_str, r.ci)).map(|r| r.name.as_str());
        let other_roots: Vec<String> = ws
            .roots
            .iter()
            .filter(|r| Some(r.name.as_str()) != cur_root)
            .map(|r| v.path(&r.path, false))
            .collect();
        let repos: Vec<String> = ws.repos.iter().map(|r| ws.display(&r.root, &r.dir, true)).collect();
        let all_leases = store::leases_ws(env.c, ws_id)?;
        let mut by_other: BTreeMap<String, Vec<&LeaseRow>> = BTreeMap::new();
        let mut mine = Vec::new();
        for l in &all_leases {
            if l.session == sid {
                mine.push(format_lease_item(ws, l));
            } else {
                by_other.entry(l.session.clone()).or_default().push(l);
            }
        }
        let others: Vec<OtherSessionIntro> = by_other
            .into_iter()
            .map(|(other_sid, ls)| {
                let s = sessions.get(&other_sid);
                let brief = s
                    .and_then(|s| s.brief.clone())
                    .or_else(|| ls.iter().find_map(|l| l.brief.clone()));
                let leases = ls.into_iter().map(|l| format_lease_item(ws, l)).collect();
                OtherSessionIntro { session: other_sid, brief, leases }
            })
            .collect();
        out.push(text::workspace_intro(mode, just_opened, &other_roots, &repos, &others, &mine));
        env.c.execute(
            "INSERT OR IGNORE INTO session_ws(session, workspace, introduced_at) VALUES(?1,?2,?3)",
            params![sid, ws_id, env.now],
        )?;
    }
    Ok(())
}

fn session_start(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    if matches!(h.source.as_deref(), Some("compact" | "resume" | "clear")) {
        env.c.execute("DELETE FROM session_ws WHERE session=?1", [sid])?;
    }
    let mut first = Vec::new();
    if h.source.as_deref() == Some("clear")
        && h.pid > 0
        && let Some(t) = take_over(env, sid, h)?
    {
        first.push(t);
    }
    let ws_ids = session_workspaces(env, sid, &[])?;
    intro_if_needed(env, sid, &ws_ids, &mut first)?;
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
        reconcile::drop_session(env.d, &o.id);
        env.c.execute("DELETE FROM interest WHERE session=?1", [&o.id])?;
        env.c.execute("DELETE FROM session_ws WHERE session=?1", [&o.id])?;
        env.c.execute("DELETE FROM read_hint WHERE session=?1 OR holder=?1", [&o.id])?;
        env.c.execute("DELETE FROM wait_notified WHERE waiter=?1 OR holder=?1", [&o.id])?;
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
        reconcile::drop_session(env.d, sid);
        env.c.execute("DELETE FROM interest WHERE session=?1", [sid])?;
        let leases = store::leases_session(env.c, sid)?;
        for ws in env.world.wss.clone() {
            let ls: Vec<&store::LeaseRow> = leases.iter().filter(|l| l.workspace == ws.row.id).collect();
            let dirty = engine::dirty_of(env, &ws, &ls);
            for l in ls {
                if dirty.contains(&(l.root.clone(), l.path_key.clone())) {
                    if let Some(since) = l.stalled_at {
                        env.event(
                            Some(&l.workspace),
                            Some(sid),
                            "lease.unstalled",
                            json!({ "root": l.root, "path": l.path, "stalled_ms": (env.now - since).max(0), "reason": "orphaned" }),
                        )?;
                    }
                    env.c.execute(
                        "UPDATE lease SET state='orphaned', stalled_at=NULL WHERE workspace=?1 AND root=?2 AND path_key=?3",
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
    let mut extra = Vec::new();
    let ws_ids = session_workspaces(env, sid, &[])?;
    intro_if_needed(env, sid, &ws_ids, &mut extra)?;
    Ok(with_context(deliver(env, sid, extra)?))
}

fn post_invocation(env: &mut Env, sid: &str) -> R<HookResponse> {
    let mut extra = Vec::new();
    let ws_ids = session_workspaces(env, sid, &[])?;
    intro_if_needed(env, sid, &ws_ids, &mut extra)?;
    Ok(with_context(deliver(env, sid, extra)?))
}

/// The main agent's turn ended (leases.md section 6): its calls are closed
/// and the session is idle until its next hook. Background commands that
/// are no longer running have their windows closed (harness.md section 5.3).
fn stop(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    if h.keep_background {
        env.c.execute(
            "DELETE FROM open_call WHERE session=?1 AND agent IS NULL AND kind='edit' AND background_id IS NULL",
            [sid],
        )?;
        env.c.execute(
            "UPDATE open_call SET background_id=call_id WHERE session=?1 AND agent IS NULL AND kind='bash' AND background_id IS NULL",
            [sid],
        )?;
        reconcile::promote_foreground_bash(env.d, sid);
    } else {
        let mut rep = reconcile::Reported::default();
        let mut said = reconcile::close_foreground(env, sid, None, &mut rep)?;
        said.extend(reconcile::close_background(env, sid, h.background_running.as_deref(), &mut rep)?);
        env.c.execute("DELETE FROM open_call WHERE session=?1 AND agent IS NULL AND background_id IS NULL", [sid])?;
        queue_hints(env, sid, said)?;
    }
    env.c.execute("UPDATE session SET state='idle', turn=turn+1 WHERE id=?1 AND state!='ended'", [sid])?;
    Ok(HookResponse::default())
}

fn agent_stop(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    if let Some(a) = &h.agent {
        let mut rep = reconcile::Reported::default();
        let said = reconcile::close_foreground(env, sid, Some(a), &mut rep)?;
        env.c.execute("DELETE FROM open_call WHERE session=?1 AND agent=?2 AND background_id IS NULL", params![sid, a])?;
        queue_hints(env, sid, said)?;
    }
    Ok(HookResponse::default())
}

/// What reconciling told the session, kept for its next hook that can
/// inject text (harness.md section 6): `Stop` and `SubagentStop` cannot.
fn queue_hints(env: &Env, sid: &str, said: Vec<String>) -> R<()> {
    for item in said {
        if !item.is_empty() {
            env.notice(sid, "hint", json!({ "text": item }))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool calls

fn pre_tool(env: &mut Env, sid: &str, h: &HookRequest) -> R<HookResponse> {
    match &h.tool {
        Some(t) if t.kind == "edit" => pre_edit(env, sid, h, t),
        Some(t) if t.kind == "bash" => pre_bash(env, sid, h, t),
        _ => Ok(HookResponse::default()),
    }
}

/// A Bash call before it runs (harness.md sections 5.1, 5.2 and 5.5): the
/// command guard, then the reconcile window. A lone `tcas` command opens
/// no window.
fn pre_bash(env: &mut Env, sid: &str, h: &HookRequest, t: &HookTool) -> R<HookResponse> {
    let Some(command) = t.command.as_deref().filter(|c| !c.trim().is_empty()) else { return Ok(HookResponse::default()) };
    let cwd = fsys::to_str(&env.cwd);
    if let Some(r) = bashguard::check(env, sid, command, &cwd)? {
        bashguard::record(env, sid, &r, command)?;
        return Ok(deny(r.reason));
    }
    let home = tcas_sys::home::user_home().map(|h| fsys::to_str(&h));
    if guard::tcas_command(&shell::parse(command, &cwd, home.as_deref())).is_some() {
        return Ok(HookResponse::default());
    }
    reconcile::open(env, sid, h.agent.as_deref(), &t.call_id, command)?;
    let bg_id = if t.background && t.background_id.is_some() {
        if let Some(id) = &t.background_id {
            reconcile::to_background(env.d, sid, &t.call_id, id);
        }
        t.background_id.clone()
    } else {
        None
    };
    env.c.execute(
        "INSERT OR REPLACE INTO open_call(session, agent, call_id, kind, paths, background_id, opened_at) \
         VALUES(?1,?2,?3,'bash',NULL,?4,?5)",
        params![sid, h.agent, t.call_id, bg_id, env.now],
    )?;
    Ok(HookResponse::default())
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
        } else {
            engine::release_if_excluded(env, &r)?;
        }
    }
    let Some(ws_id) = ws_id else { return Ok(HookResponse::default()) };
    let ws = env.world.ws(&ws_id).cloned().ok_or_else(|| Fail::bad("workspace vanished"))?;
    let mode = ws.mode(env.g.cfg.defaults.mode);
    let strict = mode == Mode::Strict;
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
            let mut reason = text::strict(&v, &new);
            if !has_session_ws(env, sid, &ws_id)? {
                let just_opened = store::session(env.c, sid)?.is_some_and(|s| ws.row.created_at > s.started_at);
                reason.push('\n');
                reason.push_str(&text::late_denial_note(mode, just_opened));
            }
            return Ok(deny(reason));
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
            reconcile::edited(env.d, sid, &inputs, env.now);
            let foreign: Vec<String> =
                acq.result.granted.iter().filter(|g| g.foreign_changes).map(|g| g.path.display.clone()).collect();
            if !foreign.is_empty() {
                env.notice(sid, "edit.foreign_changes", json!({ "workspace": ws_id, "paths": foreign }))?;
            }
            Ok(HookResponse::default())
        }
        Outcome::Denied => {
            let c = env.c;
            let mut d = text::denial(&v, &acq.result, |s| store::session(c, s).ok().flatten().and_then(|s| s.ended_at));
            if !has_session_ws(env, sid, &ws_id)? {
                let just_opened = store::session(env.c, sid)?.is_some_and(|s| ws.row.created_at > s.started_at);
                d.reason.push('\n');
                d.reason.push_str(&text::late_denial_note(mode, just_opened));
            }
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
    let tool_paths: Vec<String> = h.tool.as_ref().map(|t| t.paths.clone()).unwrap_or_default();
    let ws_ids = session_workspaces(env, sid, &tool_paths)?;
    intro_if_needed(env, sid, &ws_ids, &mut extra)?;
    if let Some(t) = &h.tool {
        let bg = t.background_id.as_deref().filter(|_| t.kind == "bash" && t.background && !t.failed);
        match bg {
            // A background command started: its window stays open under the
            // task ID (harness.md section 5.3).
            Some(id) => {
                env.c.execute(
                    "UPDATE open_call SET background_id=?3 WHERE session=?1 AND call_id=?2",
                    params![sid, t.call_id, id],
                )?;
                reconcile::to_background(env.d, sid, &t.call_id, id);
            }
            None => {
                env.c.execute("DELETE FROM open_call WHERE session=?1 AND call_id=?2", params![sid, t.call_id])?;
            }
        }
        match t.kind.as_str() {
            "edit" if !t.failed => {
                engine::mark_edited(env, sid, &t.paths, "edit")?;
                extra.extend(outside(env, sid, t)?);
                extra.extend(suggest_acquire(env, sid, t)?);
            }
            "read" => {
                touch(env, sid, &t.paths)?;
                if !t.failed && matches!(t.name.as_deref(), Some("Read" | "read_file" | "view_file")) {
                    extra.extend(read_hints(env, sid, &t.paths)?);
                }
            }
            "bash" => {
                let mut rep = reconcile::Reported::default();
                if bg.is_none() {
                    extra.extend(reconcile::close(env, sid, &t.call_id, &mut rep)?);
                }
                extra.extend(reconcile::roll_background(env, sid, &mut rep)?);
            }
            _ => {}
        }
    }
    if h.defer_context {
        queue_hints(env, sid, extra)?;
        return Ok(HookResponse::default());
    }
    Ok(with_context(deliver(env, sid, extra)?))
}

/// Hints when reading a file another session holds (harness.md section 6,
/// "读取提示"). Emitted once per (reader, workspace, root, file, holder).
fn read_hints(env: &Env, sid: &str, paths: &[String]) -> R<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let r = env.resolve(p);
        let Some(m) = r.managed else { continue };
        if r.is_dir {
            continue;
        }
        let Some(ws) = env.world.ws(&m.ws) else { continue };
        let leases = store::leases_ws(env.c, &m.ws)?;
        let mut by_holder: BTreeMap<String, LeaseRow> = BTreeMap::new();
        for l in leases {
            if l.session == sid || l.root != m.root {
                continue;
            }
            if !lease::covers(&l.path_key, l.is_prefix, &m.key, false) {
                continue;
            }
            // Prefer the file lease over its enclosing directory lease.
            by_holder
                .entry(l.session.clone())
                .and_modify(|cur| {
                    if cur.is_prefix && !l.is_prefix {
                        *cur = l.clone();
                    }
                })
                .or_insert(l);
        }
        let file_disp = ws.display(&m.root, &m.rel, false);
        let interest_path = relpath::display(&m.rel, false);
        for (holder, l) in by_holder {
            let inserted = env.c.execute(
                "INSERT OR IGNORE INTO read_hint(session, workspace, root, path_key, holder, seen_at) \
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![sid, m.ws, m.root, m.key, holder, env.now],
            )?;
            if inserted == 0 {
                continue;
            }
            env.c.execute(
                "INSERT OR IGNORE INTO interest(session, workspace, root, path, kind) VALUES(?1,?2,?3,?4,'read')",
                params![sid, m.ws, m.root, interest_path],
            )?;
            let hs = store::session(env.c, &holder)?;
            let ended = l.state == LeaseState::Orphaned || hs.as_ref().is_some_and(|s| s.state == "ended");
            let (brief_text, from_user) = match (&l.brief, hs.as_ref().and_then(|s| s.brief.as_ref())) {
                (Some(b), _) => (Some(b.as_str()), l.source == Source::Grant),
                (None, Some(b)) => (Some(b.as_str()), false),
                (None, None) => (None, false),
            };
            let dir = if l.is_prefix { Some(ws.display(&l.root, &l.path, true)) } else { None };
            out.push(text::read_hint(&file_disp, &holder, dir.as_deref(), brief_text, from_user, ended));
        }
    }
    Ok(out)
}

/// Reminds the session once after editing `hint_after_files` implicit files
/// without requesting leases first (harness.md section 6, "先申请的提醒").
fn suggest_acquire(env: &Env, sid: &str, t: &HookTool) -> R<Vec<String>> {
    let Some(s) = store::session(env.c, sid)? else { return Ok(vec![]) };
    if s.suggested_at.is_some() {
        return Ok(vec![]);
    }
    let mut ws_ids = Vec::new();
    for p in &t.paths {
        if let Some((ws, _)) = env.world.root_of(p)
            && !ws_ids.contains(&ws.row.id)
        {
            ws_ids.push(ws.row.id.clone());
        }
    }
    if ws_ids.is_empty()
        && let Some(ws) = env.world.by_cwd(&env.cwd)
    {
        ws_ids.push(ws.row.id.clone());
    }
    let mine = store::leases_session(env.c, sid)?;
    let v = Viewer::new(&env.cwd, env.now);
    for ws_id in ws_ids {
        let Some(ws) = env.world.ws(&ws_id) else { continue };
        let threshold = ws.cfg.brief.hint_after_files;
        if threshold == 0 {
            continue;
        }
        let dirs: Vec<&LeaseRow> = mine.iter().filter(|l| l.workspace == ws_id && l.is_prefix).collect();
        let implicit: Vec<&LeaseRow> = mine
            .iter()
            .filter(|l| l.workspace == ws_id && !l.is_prefix && l.source.is_implicit())
            .filter(|l| !dirs.iter().any(|d| d.root == l.root && relpath::contains(&d.path_key, &l.path_key)))
            .collect();
        if implicit.len() < threshold {
            continue;
        }
        let mut groups: BTreeMap<(String, Option<String>), Vec<&LeaseRow>> = BTreeMap::new();
        for l in &implicit {
            groups.entry((l.root.clone(), l.repo.clone())).or_default().push(l);
        }
        let mut cmd_dirs = Vec::new();
        let mut all_valid = true;
        for ((root, _repo), group) in groups {
            let common = relpath::common_dir(group.iter().map(|l| l.path.as_str()));
            if common.is_empty() || engine::tracked_under(env, ws, &root, &common).len() as u64 > ws.cfg.lease.max_dir_files {
                all_valid = false;
                break;
            }
            cmd_dirs.push(v.arg(&ws.path_ref(&root, &common, true)));
        }
        let command = if all_valid && !cmd_dirs.is_empty() {
            Some(format!(
                "tcas lease acquire {} --brief \"<one line: what you are changing and why>\"",
                cmd_dirs.join(" ")
            ))
        } else {
            None
        };
        let shown: Vec<String> = implicit.iter().map(|l| ws.display(&l.root, &l.path, false)).collect();
        env.c.execute("UPDATE session SET suggested_at=?2 WHERE id=?1", params![sid, env.now])?;
        env.event(
            Some(&ws_id),
            Some(sid),
            "lease.suggested",
            json!({ "count": implicit.len(), "paths": shown, "command": command }),
        )?;
        return Ok(vec![text::suggest_acquire(&shown, command.as_deref())]);
    }
    Ok(vec![])
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
