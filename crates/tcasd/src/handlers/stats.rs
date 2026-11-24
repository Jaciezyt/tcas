//! `tcas stats` (daemon-and-cli.md section 5, "统计"; plan.md section 5).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use tcas_core::lease::LeaseState;
use tcas_core::proto::*;
use tcas_core::relpath;
use tcas_sys::{clock, fsys};

use super::{args, with_tx};
use crate::daemon::{Daemon, Fail, R};
use crate::engine::{self, Env};
use crate::reconcile;
use crate::store;
use crate::world::Ws;

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatsArgs {
    workspace: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

/// Target checkout for `--repo`: `(root_name, repo_dir, repo_shown)`.
struct RepoFilter {
    root: String,
    dir: String,
    shown: String,
}

pub fn handle(d: &'static Daemon, req: &Request) -> R<Value> {
    let active_waits = d.waits().active_snapshot();
    with_tx(d, req, |env| compute(env, req, &active_waits))
}

fn resolve_repo(env: &Env, ws: &Ws, spec: &str) -> R<RepoFilter> {
    if ws.repos.is_empty() {
        return Err(Fail::new(code::NOT_FOUND, format!("workspace {:?} has no git checkouts", ws.row.name)));
    }
    let trimmed = if spec == "." { "." } else { spec.trim_end_matches('/') };
    for r in &ws.repos {
        let shown = reconcile::repo_shown(ws, &r.root, &r.dir);
        let shown_trim = if shown == "." { "." } else { shown.trim_end_matches('/') };
        let disp = ws.display(&r.root, &r.dir, false);
        if shown == spec
            || shown_trim == trimmed
            || (!disp.is_empty() && disp == trimmed)
            || (r.dir.is_empty() && r.root == trimmed)
        {
            return Ok(RepoFilter { root: r.root.clone(), dir: r.dir.clone(), shown });
        }
    }
    let by_dir: Vec<_> = ws.repos.iter().filter(|r| !r.dir.is_empty() && r.dir == trimmed).collect();
    if by_dir.len() == 1 {
        let r = by_dir[0];
        let shown = reconcile::repo_shown(ws, &r.root, &r.dir);
        return Ok(RepoFilter { root: r.root.clone(), dir: r.dir.clone(), shown });
    }
    let p = fsys::normalize(&fsys::absolute(&env.cwd, Path::new(spec)));
    let p = std::fs::canonicalize(&p).unwrap_or(p);
    let abs = fsys::to_str(&p);
    for r in &ws.repos {
        let r_abs = ws.repo_abs(&r.root, &r.dir);
        let r_canon = std::fs::canonicalize(&r_abs).unwrap_or(r_abs);
        if fsys::to_str(&r_canon) == abs {
            let shown = reconcile::repo_shown(ws, &r.root, &r.dir);
            return Ok(RepoFilter { root: r.root.clone(), dir: r.dir.clone(), shown });
        }
    }
    let have: Vec<String> = ws.repos.iter().map(|r| reconcile::repo_shown(ws, &r.root, &r.dir)).collect();
    Err(Fail::new(
        code::NOT_FOUND,
        format!("no checkout in workspace {:?} matches {spec:?} (have: {})", ws.row.name, have.join(", ")),
    ))
}

fn parse_display(ws: &Ws, disp: &str) -> (String, String, bool) {
    let is_dir = disp == "." || disp.ends_with('/');
    let trimmed = if disp == "." { "" } else { disp.trim_end_matches('/') };
    let default_root = ws.roots.first().map(|r| r.name.clone()).unwrap_or_default();
    if ws.multi_root() {
        if let Some((head, rest)) = trimmed.split_once('/')
            && ws.root(head).is_some()
        {
            return (head.to_string(), rest.to_string(), is_dir);
        }
        if ws.root(trimmed).is_some() {
            return (trimmed.to_string(), String::new(), true);
        }
    }
    (default_root, trimmed.to_string(), is_dir)
}

fn owning_checkout<'a>(ws: &'a Ws, root: &str, rel: &str) -> Option<&'a store::RepoRow> {
    let ci = ws.ci(root);
    let rk = relpath::key(rel, ci);
    ws.repos
        .iter()
        .filter(|r| r.root == root && relpath::contains(&relpath::key(&r.dir, ci), &rk))
        .max_by_key(|r| r.dir.len())
}

fn matches_path(ws: &Ws, rf: Option<&RepoFilter>, root: &str, rel: &str, is_dir: bool) -> bool {
    let Some(rf) = rf else { return true };
    if root != rf.root {
        return false;
    }
    if let Some(own) = owning_checkout(ws, root, rel) {
        return own.dir == rf.dir;
    }
    if is_dir {
        let ci = ws.ci(root);
        return relpath::contains(&relpath::key(rel, ci), &relpath::key(&rf.dir, ci));
    }
    false
}

fn matches_display(ws: &Ws, rf: Option<&RepoFilter>, disp: &str) -> bool {
    let (root, rel, is_dir) = parse_display(ws, disp);
    matches_path(ws, rf, &root, &rel, is_dir)
}

fn matches_repo_shown(ws: &Ws, rf: Option<&RepoFilter>, repo_field: Option<&str>) -> bool {
    let Some(rf) = rf else { return true };
    let Some(r) = repo_field else {
        return ws.repos.len() == 1 && ws.repos[0].root == rf.root && ws.repos[0].dir == rf.dir;
    };
    let a = if r == "." { "." } else { r.trim_end_matches('/') };
    let b = if rf.shown == "." { "." } else { rf.shown.trim_end_matches('/') };
    a == b
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModKind {
    Explicit,
    Implicit,
    Waited,
}

struct DirInstance {
    session: String,
    root: String,
    key: String,
    display: String,
    source: String,
    acquired_at: i64,
    released_at: Option<i64>,
    tracked_files: Option<usize>,
    in_window: bool,
    modified: BTreeSet<String>,
}

struct DenialRec {
    seq: i64,
    at: i64,
    session: String,
    wanted: Vec<(String, String, bool)>,
}

struct ReqEvent {
    seq: i64,
    at: i64,
    session: String,
    turn: Option<i64>,
    is_wait_init: bool,
    is_cli_acquire: bool,
    paths: Vec<(String, String, bool)>,
}

struct YieldRec {
    seq: i64,
    session: String,
    root: String,
    key: String,
    is_prefix: bool,
    kind: YieldKind,
    turn: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum YieldKind {
    ExplicitIdle,
    WaitedGrace,
    CommittedIdle,
}

fn paths_intersect(a: &[(String, String, bool)], b: &[(String, String, bool)]) -> bool {
    a.iter().any(|(ar, ak, ad)| {
        b.iter().any(|(br, bk, bd)| ar == br && relpath::intersects(ak, *ad, bk, *bd))
    })
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn compute(env: &mut Env, req: &Request, active_waits: &[(String, String, Vec<String>)]) -> R<Value> {
    let a: StatsArgs = args(req)?;
    let ws = env.ws_for(a.workspace.as_deref())?.clone();
    let rf = match a.repo.as_deref() {
        Some(spec) => Some(resolve_repo(env, &ws, spec)?),
        None => None,
    };
    let since_ms = match a.since.as_deref() {
        Some(s) => clock::parse_bound(s, env.now, false).map_err(Fail::bad)?,
        None => None,
    };
    let until_ms = match a.until.as_deref() {
        Some(u) => clock::parse_bound(u, env.now, true).map_err(Fail::bad)?.unwrap_or(env.now),
        None => env.now,
    };
    if let Some(s) = since_ms
        && s > until_ms
    {
        return Err(Fail::bad("--since is after --until"));
    }
    let in_win = |at: i64| since_ms.is_none_or(|s| at >= s) && at <= until_ms;

    // Clean orphaned leases and stalled transitions are synced first so the
    // event log reflects the workspace's current state.
    {
        let cur = store::leases_ws(env.c, &ws.row.id)?;
        let orphans: Vec<&store::LeaseRow> = cur.iter().filter(|l| l.state == LeaseState::Orphaned).collect();
        if !orphans.is_empty() {
            let dirty = engine::dirty_of(env, &ws, &orphans);
            for l in orphans {
                if l.is_prefix || l.repo.is_none() || !dirty.contains(&(l.root.clone(), l.path_key.clone())) {
                    engine::release(env, l, "orphan_clean", None)?;
                }
            }
        }
        engine::sync_stalled(env, &ws)?;
    }

    let evs = store::events_for_stats(env.c, &ws.row.id)?;
    let all_sessions: HashMap<String, store::SessionRow> =
        store::sessions(env.c)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let is_user = env.caller.is_user();

    let mut out = StatsResult {
        workspace_id: ws.row.id.clone(),
        workspace: ws.row.name.clone(),
        repo: rf.as_ref().map(|r| r.shown.clone()),
        since: since_ms,
        until: until_ms,
        ..StatsResult::default()
    };

    let mut sessions_seen: BTreeSet<String> = BTreeSet::new();
    let mut denial_counts: HashMap<String, usize> = HashMap::new();
    let mut denial_recs: Vec<DenialRec> = Vec::new();
    let mut req_events: Vec<ReqEvent> = Vec::new();
    let mut session_ends: HashMap<String, Vec<i64>> = HashMap::new();

    let mut open_waits: Vec<(String, Vec<String>)> = Vec::new();
    let mut wait_granted_ms: Vec<i64> = Vec::new();

    let mut dirs: Vec<DirInstance> = Vec::new();
    // (root, file_key) -> (session, source)
    let mut active_files: HashMap<(String, String), (String, String)> = HashMap::new();
    // (session, root, file_key) -> ModKind
    let mut modified_files: BTreeMap<(String, String, String), ModKind> = BTreeMap::new();
    let mut session_order: BTreeSet<String> = BTreeSet::new();

    let mut suggested_recs: Vec<(i64, String)> = Vec::new();
    // (session, batch_key) -> has_brief
    let mut upfront_batches: BTreeMap<(String, String), bool> = BTreeMap::new();

    // (root, key) -> (session, start_at)
    let mut open_orphans: HashMap<(String, String), (String, i64)> = HashMap::new();
    let mut orphan_samples: Vec<i64> = Vec::new();
    let mut open_stalled: HashMap<(String, String), (String, i64)> = HashMap::new();
    let mut stalled_samples: Vec<i64> = Vec::new();

    let mut yield_recs: Vec<YieldRec> = Vec::new();
    let mut guard_rules: HashMap<String, usize> = HashMap::new();
    let mut git_hooks: HashMap<String, (usize, usize)> = HashMap::new();
    let mut override_hooks: HashMap<String, usize> = HashMap::new();

    let parse_disp_key = |d: &str| -> (String, String, bool) {
        let (root, rel, is_dir) = parse_display(&ws, d);
        let key = relpath::key(&rel, ws.ci(&root));
        (root, key, is_dir)
    };

    let record_mod = |sid: &str,
                          root: &str,
                          rel: &str,
                          kind: ModKind,
                          at: i64,
                          dirs: &mut [DirInstance],
                          modified_files: &mut BTreeMap<(String, String, String), ModKind>,
                          session_order: &mut BTreeSet<String>| {
        let ci = ws.ci(root);
        let fkey = relpath::key(rel, ci);
        let ok_repo = matches_path(&ws, rf.as_ref(), root, rel, false);
        for d in dirs.iter_mut() {
            if d.released_at.is_none()
                && d.session == sid
                && d.root == root
                && relpath::contains(&d.key, &fkey)
                && (rf.is_none() || ok_repo)
            {
                d.modified.insert(fkey.clone());
            }
        }
        if in_win(at) && ok_repo {
            session_order.insert(sid.to_string());
            modified_files.entry((sid.to_string(), root.to_string(), fkey)).or_insert(kind);
        }
    };

    let overlaps_win = |start: i64, end: i64| -> bool {
        end >= since_ms.unwrap_or(i64::MIN) && start <= until_ms
    };

    for e in &evs {
        if e.at > until_ms {
            break;
        }
        if e.kind == "session.ended" {
            if let Some(s) = &e.session {
                session_ends.entry(s.clone()).or_default().push(e.seq);
            }
            continue;
        }
        if e.kind == "session.continued" {
            if let Some(new_sid) = e.session.as_deref()
                && let Some(old_sid) = e.payload.get("from").and_then(Value::as_str)
            {
                open_orphans.retain(|_, (holder, _)| holder != old_sid);
                for (holder, _) in active_files.values_mut() {
                    if holder == old_sid {
                        *holder = new_sid.to_string();
                    }
                }
                for d in dirs.iter_mut().filter(|d| d.released_at.is_none() && d.session == old_sid) {
                    d.session = new_sid.to_string();
                }
            }
            continue;
        }
        if e.workspace.as_deref() != Some(ws.row.id.as_str()) {
            continue;
        }
        let p = &e.payload;
        let sid = e.session.as_deref().unwrap_or("");
        let turn = p.get("turn").and_then(Value::as_i64);

        // Count workspace events in window (matching `--repo` when applicable).
        let event_matches_repo = if rf.is_none() {
            true
        } else if let Some(root) = p.get("root").and_then(Value::as_str)
            && let Some(path) = p.get("path").and_then(Value::as_str)
        {
            let is_dir = p.get("is_prefix").and_then(Value::as_bool).unwrap_or(false);
            matches_path(&ws, rf.as_ref(), root, path, is_dir)
        } else if p.get("repo").is_some() {
            matches_repo_shown(&ws, rf.as_ref(), p.get("repo").and_then(Value::as_str))
        } else if p.get("paths").is_some() {
            str_list(p, "paths").iter().any(|d| matches_display(&ws, rf.as_ref(), d))
        } else if let Some(dir) = p.get("dir").and_then(Value::as_str)
            && let Some(root) = p.get("root").and_then(Value::as_str)
        {
            matches_path(&ws, rf.as_ref(), root, dir, true)
        } else {
            false
        };

        if in_win(e.at) && event_matches_repo {
            out.events += 1;
            out.first_event_at = Some(out.first_event_at.map_or(e.at, |f| f.min(e.at)));
            out.last_event_at = Some(out.last_event_at.map_or(e.at, |l| l.max(e.at)));
            if !sid.is_empty() {
                sessions_seen.insert(sid.to_string());
            }
        }

        match e.kind.as_str() {
            "lease.granted" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let is_prefix = p.get("is_prefix").and_then(Value::as_bool).unwrap_or(false);
                let source = p.get("source").and_then(Value::as_str).unwrap_or("cli");
                let upgraded = p.get("upgraded").and_then(Value::as_bool).unwrap_or(false);
                let ci = ws.ci(root);
                let key = relpath::key(path, ci);
                let matches = matches_path(&ws, rf.as_ref(), root, path, is_prefix);

                if !sid.is_empty() && matches {
                    req_events.push(ReqEvent {
                        seq: e.seq,
                        at: e.at,
                        session: sid.to_string(),
                        turn,
                        is_wait_init: false,
                        is_cli_acquire: source == "cli",
                        paths: vec![(root.to_string(), key.clone(), is_prefix)],
                    });
                }

                if source == "cli" && !sid.is_empty() && in_win(e.at) && matches {
                    let batch = p
                        .get("req")
                        .and_then(Value::as_u64)
                        .map(|n| format!("r:{n}"))
                        .unwrap_or_else(|| format!("t:{}", e.at));
                    let has_brief = p.get("brief").and_then(Value::as_str).is_some_and(|b| !b.trim().is_empty());
                    upfront_batches
                        .entry((sid.to_string(), batch))
                        .and_modify(|v| *v |= has_brief)
                        .or_insert(has_brief);
                }

                if is_prefix {
                    if upgraded {
                        if let Some(d) = dirs.iter_mut().rev().find(|d| d.released_at.is_none() && d.root == root && d.key == key) {
                            d.source = source.to_string();
                        }
                    } else {
                        for d in dirs.iter_mut().filter(|d| d.released_at.is_none() && d.root == root && d.key == key) {
                            d.released_at = Some(e.at);
                        }
                        let in_window_dir = in_win(e.at) && matches;
                        let tracked = p
                            .get("files")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .or_else(|| in_window_dir.then(|| engine::tracked_under(env, &ws, root, path).len()));
                        dirs.push(DirInstance {
                            session: sid.to_string(),
                            root: root.to_string(),
                            key,
                            display: ws.display(root, path, true),
                            source: source.to_string(),
                            acquired_at: e.at,
                            released_at: None,
                            tracked_files: tracked,
                            in_window: in_window_dir,
                            modified: BTreeSet::new(),
                        });
                    }
                } else {
                    active_files.insert((root.to_string(), key.clone()), (sid.to_string(), source.to_string()));
                    if !upgraded && matches!(source, "edit" | "bash") {
                        let kind = if let Some(under) = p.get("under").and_then(Value::as_str) {
                            let u_src = p.get("under_source").and_then(Value::as_str).or_else(|| {
                                let uk = relpath::key(under, ci);
                                dirs.iter()
                                    .rev()
                                    .find(|d| d.released_at.is_none() && d.session == sid && d.root == root && d.key == uk)
                                    .map(|d| d.source.as_str())
                            });
                            if u_src == Some("wait") { ModKind::Waited } else { ModKind::Explicit }
                        } else {
                            ModKind::Implicit
                        };
                        record_mod(sid, root, path, kind, e.at, &mut dirs, &mut modified_files, &mut session_order);
                    }
                }
            }
            "lease.edited" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let source = p.get("source").and_then(Value::as_str).unwrap_or("cli");
                let kind = match source {
                    "wait" => ModKind::Waited,
                    "cli" | "grant" => ModKind::Explicit,
                    _ => ModKind::Implicit,
                };
                if !sid.is_empty() {
                    record_mod(sid, root, path, kind, e.at, &mut dirs, &mut modified_files, &mut session_order);
                }
            }
            "lease.denied" => {
                let wanted_raw = str_list(p, "paths");
                let blocked_raw = {
                    let b = str_list(p, "blocked");
                    if b.is_empty() { wanted_raw.clone() } else { b }
                };
                let via = p.get("via").and_then(Value::as_str).unwrap_or("edit");
                let wanted_filtered: Vec<String> =
                    wanted_raw.into_iter().filter(|d| matches_display(&ws, rf.as_ref(), d)).collect();
                let blocked_filtered: Vec<String> =
                    blocked_raw.into_iter().filter(|d| matches_display(&ws, rf.as_ref(), d)).collect();
                let parsed_wanted: Vec<(String, String, bool)> =
                    wanted_filtered.iter().map(|d| parse_disp_key(d)).collect();

                if !sid.is_empty() && !parsed_wanted.is_empty() {
                    req_events.push(ReqEvent {
                        seq: e.seq,
                        at: e.at,
                        session: sid.to_string(),
                        turn,
                        is_wait_init: false,
                        is_cli_acquire: via == "cli",
                        paths: parsed_wanted.clone(),
                    });
                }

                if via == "cli" && !sid.is_empty() && in_win(e.at) && !wanted_filtered.is_empty() {
                    let batch = p
                        .get("req")
                        .and_then(Value::as_u64)
                        .map(|n| format!("d:{n}"))
                        .unwrap_or_else(|| format!("ds:{}", e.seq));
                    let has_brief = p.get("brief").and_then(Value::as_str).is_some_and(|b| !b.trim().is_empty());
                    upfront_batches.insert((sid.to_string(), batch), has_brief);
                }

                if in_win(e.at) && !blocked_filtered.is_empty() {
                    out.denials.total += 1;
                    *out.denials.by_via.entry(via.to_string()).or_default() += 1;
                    for bp in &blocked_filtered {
                        *denial_counts.entry(bp.clone()).or_default() += 1;
                    }
                    denial_recs.push(DenialRec {
                        seq: e.seq,
                        at: e.at,
                        session: sid.to_string(),
                        wanted: if parsed_wanted.is_empty() {
                            blocked_filtered.iter().map(|d| parse_disp_key(d)).collect()
                        } else {
                            parsed_wanted
                        },
                    });
                }
            }
            "lease.released" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let is_prefix = p.get("is_prefix").and_then(Value::as_bool).unwrap_or(false);
                let reason = p.get("reason").and_then(Value::as_str).unwrap_or("");
                let source = p.get("source").and_then(Value::as_str).unwrap_or("");
                let ci = ws.ci(root);
                let key = relpath::key(path, ci);

                if !is_prefix && reason == "commit" && !sid.is_empty() {
                    let kind = match source {
                        "wait" => ModKind::Waited,
                        "cli" | "grant" => ModKind::Explicit,
                        _ => ModKind::Implicit,
                    };
                    record_mod(sid, root, path, kind, e.at, &mut dirs, &mut modified_files, &mut session_order);
                }

                if is_prefix {
                    for d in dirs.iter_mut().filter(|d| d.released_at.is_none() && d.root == root && d.key == key) {
                        d.released_at = Some(e.at);
                    }
                } else {
                    active_files.remove(&(root.to_string(), key.clone()));
                    if let Some((_, start)) = open_orphans.remove(&(root.to_string(), key.clone()))
                        && overlaps_win(start, e.at)
                    {
                        orphan_samples.push((e.at - start).max(0));
                    }
                    if let Some((_, start)) = open_stalled.remove(&(root.to_string(), key.clone()))
                        && overlaps_win(start, e.at)
                    {
                        stalled_samples.push((e.at - start).max(0));
                    }
                }

                if in_win(e.at) && !sid.is_empty() && matches_path(&ws, rf.as_ref(), root, path, is_prefix) {
                    let yk = if reason == "expired" {
                        Some(YieldKind::ExplicitIdle)
                    } else if reason == "yielded" && source == "wait" {
                        Some(YieldKind::WaitedGrace)
                    } else if reason == "committed_idle" {
                        Some(YieldKind::CommittedIdle)
                    } else {
                        None
                    };
                    if let Some(kind) = yk {
                        yield_recs.push(YieldRec {
                            seq: e.seq,
                            session: sid.to_string(),
                            root: root.to_string(),
                            key,
                            is_prefix,
                            kind,
                            turn,
                        });
                    }
                }
            }
            "lease.orphaned" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let key = relpath::key(path, ws.ci(root));
                if let Some((_, start)) = open_stalled.remove(&(root.to_string(), key.clone()))
                    && overlaps_win(start, e.at)
                {
                    stalled_samples.push((e.at - start).max(0));
                }
                if matches_path(&ws, rf.as_ref(), root, path, false) {
                    open_orphans.insert((root.to_string(), key), (sid.to_string(), e.at));
                }
            }
            "lease.readopted" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let key = relpath::key(path, ws.ci(root));
                if let Some((_, start)) = open_orphans.remove(&(root.to_string(), key))
                    && overlaps_win(start, e.at)
                {
                    orphan_samples.push((e.at - start).max(0));
                }
            }
            "lease.transferred" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let key = relpath::key(path, ws.ci(root));
                if let Some((_, start)) = open_orphans.remove(&(root.to_string(), key.clone()))
                    && overlaps_win(start, e.at)
                {
                    orphan_samples.push((e.at - start).max(0));
                }
                if let Some((_, start)) = open_stalled.remove(&(root.to_string(), key.clone()))
                    && overlaps_win(start, e.at)
                {
                    stalled_samples.push((e.at - start).max(0));
                }
                if let Some(( prev_sid, src )) = active_files.get(&(root.to_string(), key.clone())).cloned()
                    && prev_sid != sid
                    && !sid.is_empty()
                {
                    active_files.insert((root.to_string(), key.clone()), (sid.to_string(), src));
                }
                for d in dirs.iter_mut().filter(|d| d.released_at.is_none() && d.root == root && d.key == key) {
                    d.released_at = Some(e.at);
                }
            }
            "lease.stalled" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let key = relpath::key(path, ws.ci(root));
                let since = p.get("since").and_then(Value::as_i64).unwrap_or(e.at);
                if matches_path(&ws, rf.as_ref(), root, path, false) {
                    open_stalled.entry((root.to_string(), key)).or_insert((sid.to_string(), since));
                }
            }
            "lease.unstalled" => {
                let Some(root) = p.get("root").and_then(Value::as_str) else { continue };
                let Some(path) = p.get("path").and_then(Value::as_str) else { continue };
                let key = relpath::key(path, ws.ci(root));
                if let Some((_, start)) = open_stalled.remove(&(root.to_string(), key)) {
                    if overlaps_win(start, e.at) {
                        let dur = p.get("stalled_ms").and_then(Value::as_i64).unwrap_or(e.at - start).max(0);
                        stalled_samples.push(dur);
                    }
                } else if in_win(e.at)
                    && matches_path(&ws, rf.as_ref(), root, path, false)
                    && let Some(dur) = p.get("stalled_ms").and_then(Value::as_i64)
                {
                    stalled_samples.push(dur.max(0));
                }
            }
            "wait.started" => {
                let ps: Vec<String> = str_list(p, "paths").into_iter().filter(|d| matches_display(&ws, rf.as_ref(), d)).collect();
                if !sid.is_empty() && !ps.is_empty() {
                    open_waits.push((sid.to_string(), ps.clone()));
                    req_events.push(ReqEvent {
                        seq: e.seq,
                        at: e.at,
                        session: sid.to_string(),
                        turn,
                        is_wait_init: true,
                        is_cli_acquire: false,
                        paths: ps.iter().map(|d| parse_disp_key(d)).collect(),
                    });
                }
            }
            "wait.granted" | "wait.timeout" | "wait.deadlock" | "wait.cancelled" => {
                let raw_ps = str_list(p, "paths");
                let ps: Vec<String> = raw_ps.iter().filter(|d| matches_display(&ws, rf.as_ref(), d)).cloned().collect();
                if ps.is_empty() && (rf.is_some() || !raw_ps.is_empty()) {
                    continue;
                }
                let at_reg = p.get("at").and_then(Value::as_str) == Some("registration");
                let had_open = if at_reg {
                    false
                } else if !ps.is_empty()
                    && let Some(idx) = open_waits.iter().position(|(s, wps)| s == sid && *wps == ps)
                {
                    open_waits.remove(idx);
                    true
                } else if ps.is_empty()
                    && let Some(idx) = open_waits.iter().position(|(s, _)| s == sid)
                {
                    open_waits.remove(idx);
                    true
                } else {
                    false
                };
                if !had_open && !sid.is_empty() && e.kind != "wait.cancelled" {
                    req_events.push(ReqEvent {
                        seq: e.seq,
                        at: e.at,
                        session: sid.to_string(),
                        turn,
                        is_wait_init: true,
                        is_cli_acquire: false,
                        paths: ps.iter().map(|d| parse_disp_key(d)).collect(),
                    });
                }
                if in_win(e.at) {
                    let ms = p.get("waited_ms").and_then(Value::as_i64).unwrap_or(0).max(0);
                    match e.kind.as_str() {
                        "wait.granted" => {
                            out.waits.granted += 1;
                            wait_granted_ms.push(ms);
                        }
                        "wait.timeout" => out.waits.timeout += 1,
                        "wait.deadlock" => out.waits.deadlock += 1,
                        "wait.cancelled" => out.waits.cancelled += 1,
                        _ => {}
                    }
                }
            }
            "lease.suggested" => {
                let ps = str_list(p, "paths");
                let ok = rf.is_none() || ps.iter().any(|d| matches_display(&ws, rf.as_ref(), d));
                if in_win(e.at) && ok && !sid.is_empty() {
                    suggested_recs.push((e.seq, sid.to_string()));
                }
            }
            "commit" => {
                let by = p.get("by").and_then(Value::as_str).or((!sid.is_empty()).then_some(sid));
                if let Some(committer) = by {
                    for disp in str_list(p, "paths") {
                        let (root, rel, is_dir) = parse_display(&ws, &disp);
                        if is_dir {
                            continue;
                        }
                        let ci = ws.ci(&root);
                        let key = relpath::key(&rel, ci);
                        let kind = if let Some((holder, src)) = active_files.get(&(root.clone(), key.clone()))
                            && holder == committer
                        {
                            Some(match src.as_str() {
                                "wait" => ModKind::Waited,
                                "cli" | "grant" => ModKind::Explicit,
                                _ => ModKind::Implicit,
                            })
                        } else {
                            dirs.iter()
                                .rev()
                                .find(|d| d.released_at.is_none() && d.session == committer && d.root == root && relpath::contains(&d.key, &key))
                                .map(|d| if d.source == "wait" { ModKind::Waited } else { ModKind::Explicit })
                        };
                        if let Some(k) = kind {
                            record_mod(committer, &root, &rel, k, e.at, &mut dirs, &mut modified_files, &mut session_order);
                        }
                    }
                }
            }
            "bash.overreach" => {
                if in_win(e.at) && str_list(p, "paths").iter().any(|d| matches_display(&ws, rf.as_ref(), d)) {
                    out.bash.overreach += 1;
                }
            }
            "bash.unattributed" => {
                if in_win(e.at) && str_list(p, "paths").iter().any(|d| matches_display(&ws, rf.as_ref(), d)) {
                    out.bash.unattributed += 1;
                }
            }
            "bash.blocked" => {
                if in_win(e.at) && matches_repo_shown(&ws, rf.as_ref(), p.get("repo").and_then(Value::as_str)) {
                    let rule = p.get("rule").and_then(Value::as_str).unwrap_or("unknown");
                    out.guard_blocked.total += 1;
                    if rule == "dynamic" {
                        out.guard_blocked.dynamic += 1;
                    }
                    *guard_rules.entry(rule.to_string()).or_default() += 1;
                }
            }
            "git.blocked" => {
                if in_win(e.at) && matches_repo_shown(&ws, rf.as_ref(), p.get("repo").and_then(Value::as_str)) {
                    let hook = p.get("hook").and_then(Value::as_str).unwrap_or("unknown");
                    let from_session = !sid.is_empty() || p.get("by").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                    out.git_blocked.total += 1;
                    let entry = git_hooks.entry(hook.to_string()).or_default();
                    entry.0 += 1;
                    if from_session {
                        out.git_blocked.missed_by_guard += 1;
                        entry.1 += 1;
                    } else {
                        out.git_blocked.by_user += 1;
                    }
                }
            }
            "git.override" => {
                if in_win(e.at) && matches_repo_shown(&ws, rf.as_ref(), p.get("repo").and_then(Value::as_str)) {
                    let hook = p.get("hook").and_then(Value::as_str).unwrap_or("unknown");
                    out.git_overrides.total += 1;
                    *override_hooks.entry(hook.to_string()).or_default() += 1;
                }
            }
            "workspace.closed" => {
                for d in dirs.iter_mut().filter(|d| d.released_at.is_none()) {
                    d.released_at = Some(e.at);
                }
                active_files.clear();
                for (_, (_, start)) in open_orphans.drain() {
                    if overlaps_win(start, e.at) {
                        orphan_samples.push((e.at - start).max(0));
                    }
                }
                for (_, (_, start)) in open_stalled.drain() {
                    if overlaps_win(start, e.at) {
                        stalled_samples.push((e.at - start).max(0));
                    }
                }
            }
            _ => {}
        }
    }

    // Live workspace state when `until_ms` covers `now`: uncommitted manual
    // edits under explicit/waited leases, still-open orphans/stalled leases,
    // and currently waiting sessions.
    if until_ms >= env.now - 1000 {
        let cur_leases = store::leases_ws(env.c, &ws.row.id)?;
        let file_refs: Vec<&store::LeaseRow> = cur_leases.iter().filter(|l| !l.is_prefix).collect();
        let dirty_now = if file_refs.is_empty() { HashSet::new() } else { engine::dirty_of(env, &ws, &file_refs) };

        for l in &cur_leases {
            if matches_path(&ws, rf.as_ref(), &l.root, &l.path, l.is_prefix) && in_win(l.touched_at.max(l.acquired_at)) {
                sessions_seen.insert(l.session.clone());
            }
            if l.is_prefix {
                continue;
            }
            let is_dirty = dirty_now.contains(&(l.root.clone(), l.path_key.clone()));
            if is_dirty {
                let kind = match l.source {
                    tcas_core::lease::Source::Cli | tcas_core::lease::Source::Grant => ModKind::Explicit,
                    tcas_core::lease::Source::Wait => ModKind::Waited,
                    _ => ModKind::Implicit,
                };
                record_mod(
                    &l.session,
                    &l.root,
                    &l.path,
                    kind,
                    l.edited_at.unwrap_or(l.touched_at),
                    &mut dirs,
                    &mut modified_files,
                    &mut session_order,
                );
            }
            if !matches_path(&ws, rf.as_ref(), &l.root, &l.path, false) {
                continue;
            }
            if l.state == LeaseState::Orphaned {
                let start = open_orphans
                    .remove(&(l.root.clone(), l.path_key.clone()))
                    .map(|(_, s)| s)
                    .unwrap_or(l.touched_at);
                if start <= until_ms {
                    out.orphaned.current += 1;
                    orphan_samples.push((until_ms - start).max(0));
                }
            }
            if let Some(stalled_since) = l.stalled_at {
                let start = open_stalled
                    .remove(&(l.root.clone(), l.path_key.clone()))
                    .map(|(_, s)| s)
                    .unwrap_or(stalled_since);
                if start <= until_ms {
                    out.stalled.current += 1;
                    stalled_samples.push((until_ms - start).max(0));
                }
            }
        }

        for (w_ws, w_sid, wanted) in active_waits {
            if w_ws == &ws.row.id && wanted.iter().any(|d| matches_display(&ws, rf.as_ref(), d)) {
                out.waits.active += 1;
                sessions_seen.insert(w_sid.clone());
            }
        }
    } else {
        for (_, (_, start)) in open_orphans {
            if start <= until_ms {
                orphan_samples.push((until_ms - start).max(0));
            }
        }
        for (_, (_, start)) in open_stalled {
            if start <= until_ms {
                stalled_samples.push((until_ms - start).max(0));
            }
        }
    }

    out.sessions = sessions_seen.len();

    // 1. Denials: by_path and waited vs not_waited.
    let mut by_path: Vec<PathCount> =
        denial_counts.into_iter().map(|(path, count)| PathCount { path, count }).collect();
    by_path.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
    out.denials.by_path = by_path;

    for (i, d) in denial_recs.iter().enumerate() {
        let end_end = session_ends
            .get(&d.session)
            .and_then(|seqs| seqs.iter().copied().find(|&s| s > d.seq))
            .unwrap_or(i64::MAX);
        let next_denial = denial_recs[i + 1..]
            .iter()
            .find(|d2| d2.session == d.session && paths_intersect(&d.wanted, &d2.wanted))
            .map(|d2| d2.seq)
            .unwrap_or(i64::MAX);
        let bound = end_end.min(next_denial);
        let waited = req_events.iter().any(|r| {
            r.is_wait_init
                && r.session == d.session
                && r.seq > d.seq
                && r.seq < bound
                && r.at - d.at <= 30 * 60_000
                && paths_intersect(&d.wanted, &r.paths)
        });
        if waited {
            out.denials.waited_after += 1;
        } else {
            out.denials.not_waited_after += 1;
        }
    }

    // 2. Waits.
    out.waits.total =
        out.waits.granted + out.waits.timeout + out.waits.deadlock + out.waits.cancelled + out.waits.active;
    out.waits.granted_durations = DurationStats::from_samples(wait_granted_ms);

    // 3. Modified files per session.
    let mut by_sess: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for ((sid, _, _), kind) in &modified_files {
        let e = by_sess.entry(sid.clone()).or_default();
        match kind {
            ModKind::Explicit => {
                e.0 += 1;
                out.files.explicit += 1;
            }
            ModKind::Implicit => {
                e.1 += 1;
                out.files.implicit += 1;
            }
            ModKind::Waited => {
                e.2 += 1;
                out.files.waited += 1;
            }
        }
        out.files.total += 1;
    }
    for sid in session_order {
        let (explicit, implicit, waited) = by_sess.remove(&sid).unwrap_or_default();
        let total = explicit + implicit + waited;
        if total == 0 {
            continue;
        }
        let title = if is_user { all_sessions.get(&sid).and_then(|s| s.title.clone()) } else { None };
        out.files_by_session.push(SessionFileStats { session: sid, title, total, explicit, implicit, waited });
    }

    // 4. Directory leases.
    for d in dirs {
        if !d.in_window {
            continue;
        }
        out.directory_leases.push(DirLeaseStats {
            session: d.session,
            path: d.display,
            source: d.source,
            acquired_at: d.acquired_at,
            tracked_files: d.tracked_files,
            modified_files: d.modified.len(),
        });
    }

    // 5. Acquire reminders (`lease.suggested`).
    out.suggestions.suggested = suggested_recs.len();
    for (seq, sid) in &suggested_recs {
        let end_seq = session_ends
            .get(sid)
            .and_then(|seqs| seqs.iter().copied().find(|&s| s > *seq))
            .unwrap_or(i64::MAX);
        let did_acquire = req_events.iter().any(|r| r.is_cli_acquire && &r.session == sid && r.seq > *seq && r.seq < end_seq);
        if did_acquire {
            out.suggestions.acquired_after += 1;
        }
    }

    // 6. Upfront `--brief` ratio.
    out.upfront_acquires.total = upfront_batches.len();
    out.upfront_acquires.with_brief = upfront_batches.values().filter(|&&b| b).count();
    out.upfront_acquires.without_brief = out.upfront_acquires.total - out.upfront_acquires.with_brief;

    // 8. Orphaned & stalled durations.
    out.orphaned.total = orphan_samples.len();
    out.orphaned.durations = DurationStats::from_samples(orphan_samples);
    out.stalled.total = stalled_samples.len();
    out.stalled.durations = DurationStats::from_samples(stalled_samples);

    // 9 & 10. Yields and re-requests by original holder.
    for y in &yield_recs {
        let y_path = [(y.root.clone(), y.key.clone(), y.is_prefix)];
        match y.kind {
            YieldKind::ExplicitIdle | YieldKind::WaitedGrace => {
                out.yields.expired.total += 1;
                if y.kind == YieldKind::ExplicitIdle {
                    out.yields.expired.explicit_idle += 1;
                } else {
                    out.yields.expired.waited_grace += 1;
                }
                let reacquired = req_events.iter().any(|r| {
                    r.session == y.session && r.seq > y.seq && paths_intersect(&y_path, &r.paths)
                });
                if reacquired {
                    out.yields.expired.reacquired += 1;
                    if y.kind == YieldKind::ExplicitIdle {
                        out.yields.expired.explicit_idle_reacquired += 1;
                    } else {
                        out.yields.expired.waited_grace_reacquired += 1;
                    }
                }
            }
            YieldKind::CommittedIdle => {
                out.yields.committed_idle.total += 1;
                let end_seq = session_ends
                    .get(&y.session)
                    .and_then(|seqs| seqs.iter().copied().find(|&s| s > y.seq))
                    .unwrap_or(i64::MAX);
                let reacquired_next = req_events.iter().any(|r| {
                    if r.session != y.session || r.seq <= y.seq || r.seq >= end_seq {
                        return false;
                    }
                    if !paths_intersect(&y_path, &r.paths) {
                        return false;
                    }
                    match (y.turn, r.turn) {
                        (Some(yt), Some(rt)) if yt > 0 => rt == yt,
                        _ => true,
                    }
                });
                if reacquired_next {
                    out.yields.committed_idle.reacquired_next_turn += 1;
                }
            }
        }
    }

    // 11. Command guard rules.
    let mut rules: Vec<RuleCount> =
        guard_rules.into_iter().map(|(rule, count)| RuleCount { rule, count }).collect();
    rules.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.rule.cmp(&b.rule)));
    out.guard_blocked.by_rule = rules;

    // 12 & 13. Git hook blocks and overrides.
    let mut hooks: Vec<HookCount> = git_hooks
        .into_iter()
        .map(|(hook, (count, missed_by_guard))| HookCount { hook, count, missed_by_guard })
        .collect();
    hooks.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.hook.cmp(&b.hook)));
    out.git_blocked.by_hook = hooks;

    let mut ov_hooks: Vec<HookCount> = override_hooks
        .into_iter()
        .map(|(hook, count)| HookCount { hook, count, missed_by_guard: 0 })
        .collect();
    ov_hooks.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.hook.cmp(&b.hook)));
    out.git_overrides.by_hook = ov_hooks;

    Ok(serde_json::to_value(out).unwrap())
}
