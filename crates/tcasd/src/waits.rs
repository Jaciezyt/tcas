//! Waits, the wait graph and deadlock detection (leases.md section 7).
//! Waits live in memory; `tcas lease wait` re-registers after a restart.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;
use tcas_core::lease::Source;
use tcas_core::proto::{AcquireResult, CycleStep, Outcome, PathRef};
use tcas_core::wait::{self, Graph};

use crate::daemon::{Caller, Daemon, R, log};
use crate::engine::{self, AcqReq, Env};
use crate::world::World;

pub struct WaitEntry {
    pub id: u64,
    pub session: String,
    pub ws: String,
    pub cwd: PathBuf,
    pub inputs: Vec<String>,
    /// Display forms of the managed paths, for status.
    pub wanted: Vec<String>,
    pub brief: Option<String>,
    pub started_at: i64,
    pub deadline: i64,
    /// (holder, wanted path, held path), from the latest evaluation.
    pub edges: Vec<(String, PathRef, PathRef)>,
    pub next_change: Option<i64>,
    /// Last commit touching each wanted file when the wait started.
    pub snapshot: Vec<(PathRef, Option<String>)>,
    pub tx: mpsc::Sender<AcquireResult>,
}

#[derive(Default)]
pub struct Waits {
    pub next_id: u64,
    pub list: Vec<WaitEntry>,
}

impl Waits {
    pub fn graph(&self) -> Graph {
        let mut g = Graph::new();
        for w in &self.list {
            for (h, _, _) in &w.edges {
                g.entry(w.session.clone()).or_default().insert(h.clone());
            }
        }
        g
    }

    pub fn sessions(&self) -> BTreeSet<String> {
        self.list.iter().map(|w| w.session.clone()).collect()
    }

    pub fn summary(&self) -> HashMap<String, Vec<String>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for w in &self.list {
            out.entry(w.session.clone()).or_default().extend(w.wanted.iter().cloned());
        }
        out
    }

    pub fn remove(&mut self, id: u64) -> Option<WaitEntry> {
        let i = self.list.iter().position(|w| w.id == id)?;
        Some(self.list.remove(i))
    }
}

/// Edges of all waits plus a prospective one, as a graph.
pub fn graph_with(waits: &Waits, session: &str, edges: &[(String, PathRef, PathRef)]) -> Graph {
    let mut g = waits.graph();
    for (h, _, _) in edges {
        g.entry(session.to_string()).or_default().insert(h.clone());
    }
    g
}

fn edge<'a>(waits: &'a Waits, extra: Option<(&'a str, &'a [(String, PathRef, PathRef)])>, from: &str, to: &str) -> Option<&'a (String, PathRef, PathRef)> {
    if let Some((s, edges)) = extra
        && s == from
        && let Some(e) = edges.iter().find(|e| e.0 == to)
    {
        return Some(e);
    }
    waits.list.iter().filter(|w| w.session == from).flat_map(|w| w.edges.iter()).find(|e| e.0 == to)
}

/// Describes a cycle through `me`: each other session, what it holds that the
/// previous one wants, and what it waits for.
pub fn describe(waits: &Waits, me: &str, extra: Option<(&str, &[(String, PathRef, PathRef)])>, cycle: &[String]) -> Vec<CycleStep> {
    // cycle = [n1, n2, ..., me]; edges me→n1→...→me.
    let mut nodes = vec![me.to_string()];
    nodes.extend(cycle.iter().cloned());
    let mut out = Vec::new();
    for i in 1..nodes.len() - 1 {
        let prev = &nodes[i - 1];
        let cur = &nodes[i];
        let next = &nodes[i + 1];
        let (Some(inb), Some(outb)) = (edge(waits, extra, prev, cur), edge(waits, extra, cur, next)) else { continue };
        out.push(CycleStep {
            session: cur.clone(),
            holds: inb.2.clone(),
            waits_for: outb.1.clone(),
            next: if next == me { String::new() } else { next.clone() },
        });
    }
    out
}

/// One evaluation of every wait, in the order they started.
pub fn evaluate(d: &Daemon) {
    let mut waits = d.waits();
    if waits.list.is_empty() {
        return;
    }
    let now = tcas_sys::clock::now_ms();
    waits.list.sort_by_key(|w| (w.started_at, w.id));
    let ids: Vec<u64> = waits.list.iter().map(|w| w.id).collect();
    for id in ids {
        let Some(idx) = waits.list.iter().position(|w| w.id == id) else { continue };
        let w = &waits.list[idx];
        let res = attempt(d, w, now);
        match res {
            Ok(Attempt::Granted(mut r)) => {
                let w = waits.remove(id).expect("present");
                r.waited_ms = Some(now - w.started_at);
                changed_while_waiting(d, &w, &mut r);
                finish(d, &w, r, "wait.granted");
            }
            Ok(Attempt::Gone) => {
                let w = waits.remove(id).expect("present");
                let mut r = AcquireResult::new(Outcome::Cancelled);
                r.refusal = Some("the waiting session has ended".into());
                finish(d, &w, r, "wait.cancelled");
            }
            Ok(Attempt::Denied(mut r, edges, next)) => {
                if now >= waits.list[idx].deadline {
                    let w = waits.remove(id).expect("present");
                    r.outcome = Outcome::Timeout;
                    r.waited_ms = Some(now - w.started_at);
                    finish(d, &w, r, "wait.timeout");
                } else {
                    let w = &mut waits.list[idx];
                    w.edges = edges;
                    w.next_change = next;
                }
            }
            Err(e) => log(format!("wait {id}: {}", e.message)),
        }
    }
    // A cycle that formed after registration: end the latest wait in it.
    while let Some(cycle) = wait::any_cycle(&waits.graph()) {
        let Some(victim) = waits
            .list
            .iter()
            .filter(|w| cycle.contains(&w.session))
            .max_by_key(|w| (w.started_at, w.id))
            .map(|w| w.id)
        else {
            break;
        };
        let w = waits.remove(victim).expect("present");
        let g = waits.graph();
        let mut r = AcquireResult::new(Outcome::Deadlock);
        r.session = w.session.clone();
        let mut g2 = g.clone();
        for (h, _, _) in &w.edges {
            g2.entry(w.session.clone()).or_default().insert(h.clone());
        }
        if let Some(c) = wait::cycle_through(&g2, &w.session) {
            r.cycle = describe(&waits, &w.session, Some((&w.session, &w.edges)), &c);
        }
        r.waited_ms = Some(now - w.started_at);
        finish(d, &w, r, "wait.deadlock");
    }
}

enum Attempt {
    Granted(AcquireResult),
    Denied(AcquireResult, Vec<(String, PathRef, PathRef)>, Option<i64>),
    Gone,
}

fn attempt(d: &Daemon, w: &WaitEntry, now: i64) -> R<Attempt> {
    let mut conn = d.db();
    let tx = conn.transaction()?;
    if let Some(s) = crate::store::session(&tx, &w.session)?
        && s.state == "ended"
    {
        return Ok(Attempt::Gone);
    }
    let env = Env {
        d,
        c: &tx,
        world: World::load(&tx)?,
        g: d.global(),
        now,
        caller: Caller::Session(w.session.clone()),
        cwd: w.cwd.clone(),
        waiting: HashMap::new(),
    };
    let acq = engine::acquire(
        &env,
        &AcqReq {
            session: &w.session,
            inputs: &w.inputs,
            source: Source::Wait,
            brief: w.brief.clone(),
            check_size: true,
            record_denial: false,
        },
    )?;
    let out = match acq.result.outcome {
        Outcome::Granted => Attempt::Granted(acq.result),
        _ => Attempt::Denied(acq.result, acq.holders, acq.next_change),
    };
    tx.commit()?;
    Ok(out)
}

fn changed_while_waiting(d: &Daemon, w: &WaitEntry, r: &mut AcquireResult) {
    for (p, before) in &w.snapshot {
        let after = last_commit(d, &p.abs);
        if after.is_some() && after != *before
            && let Some(g) = r.granted.iter_mut().find(|g| g.path.abs == p.abs)
        {
            g.changed_in = after;
        }
    }
}

/// The short hash of the last commit touching a file, if it is in a checkout.
pub fn last_commit(d: &Daemon, abs: &str) -> Option<String> {
    let p = std::path::Path::new(abs);
    let dir = p.parent()?;
    let dir = if dir.exists() { dir } else { return None };
    let o = d.git.cmd(dir).args(["log", "-1", "--format=%h", "--"]).arg(p.file_name()?).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() && !s.is_empty() { Some(s) } else { None }
}

fn finish(d: &Daemon, w: &WaitEntry, r: AcquireResult, kind: &str) {
    let now = tcas_sys::clock::now_ms();
    let paths: Vec<String> = r.wanted.iter().map(|p| p.display.clone()).collect();
    let mut payload = json!({ "paths": paths, "waited_ms": now - w.started_at });
    if kind == "wait.deadlock" {
        payload["cycle"] = serde_json::to_value(&r.cycle).unwrap_or_default();
    }
    let conn = d.db();
    let _ = crate::store::event(&conn, now, Some(&w.ws), Some(&w.session), kind, payload.clone());
    if kind != "wait.cancelled" {
        let _ = crate::store::notice(&conn, now, &w.session, kind, json!({ "workspace": w.ws, "result": r }));
    }
    drop(conn);
    let _ = w.tx.send(r);
}

/// The evaluator thread: runs on every kick, every 5 seconds while waits
/// exist, and when a time-bound hold or a deadline ends.
pub fn run(d: &'static Daemon) {
    loop {
        let timeout = {
            let waits = d.waits();
            if waits.list.is_empty() {
                Duration::from_secs(3600)
            } else {
                let now = tcas_sys::clock::now_ms();
                let mut t = now + 5000;
                for w in &waits.list {
                    t = t.min(w.deadline);
                    if let Some(n) = w.next_change {
                        t = t.min(n);
                    }
                }
                Duration::from_millis((t - now).clamp(10, 5000) as u64)
            }
        };
        {
            let mut k = d.kick_flag.lock().unwrap_or_else(|e| e.into_inner());
            if !*k {
                k = d.kick_cv.wait_timeout(k, timeout).unwrap_or_else(|e| e.into_inner()).0;
            }
            *k = false;
        }
        if d.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        evaluate(d);
    }
}
