//! Shared daemon state, errors and caller identity.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, MutexGuard, RwLock};
use std::time::SystemTime;

use rusqlite::Connection;
use serde_json::{Value, json};
use tcas_core::config::GlobalConfig;
use tcas_core::exempt::ExemptDir;
use tcas_core::proto::code;
use tcas_sys::home::Home;

use crate::git::Git;
use crate::store;
use crate::waits::Waits;

#[derive(Debug)]
pub struct Fail {
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
}

impl Fail {
    pub fn new(code: &str, message: impl Into<String>) -> Fail {
        Fail { code: code.into(), message: message.into(), detail: None }
    }

    pub fn bad(message: impl Into<String>) -> Fail {
        Fail::new(code::BAD_REQUEST, message)
    }

    pub fn refused(message: impl Into<String>) -> Fail {
        Fail::new(code::REFUSED, message)
    }

    /// Beyond what a session may do (exit code 6). `reason` is a sentence.
    pub fn user_only(reason: &str) -> Fail {
        Fail::new(code::USER_ONLY, format!("{reason} This is for the user to do: tell them what you need."))
    }

    pub fn with_detail(mut self, d: Value) -> Fail {
        self.detail = Some(d);
        self
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Fail {
        Fail::new(code::INTERNAL, format!("database: {e}"))
    }
}

pub type R<T> = Result<T, Fail>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    User,
    Session(String),
}

impl Caller {
    pub fn session(&self) -> Option<&str> {
        match self {
            Caller::Session(s) => Some(s),
            Caller::User => None,
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Caller::User)
    }

    /// `user` or the session ID, as stored in `opened_by` and `added_by`.
    pub fn label(&self) -> String {
        match self {
            Caller::User => "user".into(),
            Caller::Session(s) => s.clone(),
        }
    }

    pub fn require_user(&self, what: &str) -> R<()> {
        if self.is_user() { Ok(()) } else { Err(Fail::user_only(&format!("{what} is only for the user."))) }
    }

    pub fn require_session(&self, what: &str) -> R<&str> {
        match self {
            Caller::Session(s) => Ok(s),
            Caller::User => Err(Fail::new(
                code::SESSION_ONLY,
                format!(
                    "{what} is for sessions; the user is not bound by leases. To act as a session, set \
                     TCAS_SESSION."
                ),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Global {
    pub cfg: GlobalConfig,
    pub exempt: Vec<ExemptDir>,
    pub problems: Vec<String>,
    mtime: Option<SystemTime>,
}

pub struct Daemon {
    pub home: Home,
    pub db: Mutex<Connection>,
    pub git: Git,
    pub global: RwLock<Global>,
    pub waits: Mutex<Waits>,
    pub kick_flag: Mutex<bool>,
    pub kick_cv: Condvar,
    pub stopping: AtomicBool,
    pub started_at: i64,
    pub pid: i32,
    pub socket: PathBuf,
    /// Bash reconcile windows, in memory only (harness.md section 7).
    pub windows: Mutex<crate::reconcile::Windows>,
}

pub fn load_global(home: &Home) -> Global {
    let path = home.config();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut problems = Vec::new();
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => match tcas_core::config::parse_global(&text) {
            Ok(c) => c,
            Err(e) => {
                problems.push(format!("{}: {e}", path.display()));
                GlobalConfig::default()
            }
        },
        Err(_) => GlobalConfig::default(),
    };
    let (exempt, p) = tcas_sys::exempt::global_list(&cfg, home);
    problems.extend(p);
    Global { cfg, exempt, problems, mtime }
}

impl Daemon {
    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn waits(&self) -> MutexGuard<'_, Waits> {
        self.waits.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Taken after the database lock, never before it.
    pub fn windows(&self) -> MutexGuard<'_, crate::reconcile::Windows> {
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The global configuration, re-read when the file changed. The second
    /// value says it was re-read, so `workspaces.json` needs rewriting.
    pub fn global_checked(&self) -> (Global, bool) {
        let mtime = std::fs::metadata(self.home.config()).and_then(|m| m.modified()).ok();
        {
            let g = self.global.read().unwrap_or_else(|e| e.into_inner());
            if g.mtime == mtime {
                return (g.clone(), false);
            }
        }
        let fresh = load_global(&self.home);
        *self.global.write().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
        (fresh, true)
    }

    pub fn global(&self) -> Global {
        self.global_checked().0
    }

    /// Wakes the wait evaluator.
    pub fn kick(&self) {
        let mut k = self.kick_flag.lock().unwrap_or_else(|e| e.into_inner());
        *k = true;
        self.kick_cv.notify_all();
    }
}

pub fn log(msg: impl AsRef<str>) {
    eprintln!("{} {}", tcas_sys::clock::stamp(tcas_sys::clock::now_ms()), msg.as_ref());
}

/// Session IDs look like `claude:7958…`; one set by hand may have no prefix.
pub fn harness_of(id: &str) -> String {
    match id.split_once(':') {
        Some((h, _)) if !h.is_empty() => h.to_string(),
        _ => "manual".into(),
    }
}

/// Works out who sent a request (harness.md section 2) and records that the
/// session was seen. A session's request brings it back to `active` and
/// readopts its orphaned leases (leases.md section 6, invariant 6).
pub fn identify(
    c: &Connection,
    home: &Home,
    session: Option<&str>,
    ancestors: &[tcas_core::proto::ProcId],
    cwd: &str,
    now: i64,
) -> R<Caller> {
    let sid = match session.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s.to_string()),
        None => {
            // The newest live session of the nearest registered ancestor
            // (after /clear the process also has the ended session before it).
            let mut found = None;
            if !ancestors.is_empty() {
                let mut all = store::sessions(c)?;
                // Live ones first, newest first.
                all.sort_by_key(|s| (s.state == "ended", -s.started_at));
                'outer: for a in ancestors {
                    for s in &all {
                        if let (Some(pid), Some(st)) = (s.pid, s.pid_started_at)
                            && a.pid == pid
                            && (a.started_at - st).abs() <= 1
                        {
                            found = Some(s.id.clone());
                            break 'outer;
                        }
                    }
                }
            }
            found
        }
    };
    let Some(sid) = sid else { return Ok(Caller::User) };
    seen(c, home, &sid, &Seen { cwd: Some(cwd), pid: None, create: true, activate: true, via: "cli" }, now)?;
    Ok(Caller::Session(sid))
}

/// What a request tells about its session.
pub struct Seen<'a> {
    /// The session's working directory (not updated from subagents).
    pub cwd: Option<&'a str>,
    /// The harness process, from hooks.
    pub pid: Option<tcas_core::proto::ProcId>,
    /// Create the record if there is none.
    pub create: bool,
    /// Bring the session back to `active` (and an ended one back to life).
    /// `false` only updates the time it was last seen.
    pub activate: bool,
    /// `cli` or `hook`, for the event.
    pub via: &'a str,
}

/// Records that a session was seen: creates it, or brings it back to
/// `active` and readopts its orphaned leases (leases.md section 6,
/// invariant 6). Returns the record, or `None` when it does not exist and
/// `create` is false.
pub fn seen(c: &Connection, home: &Home, sid: &str, s: &Seen, now: i64) -> R<Option<store::SessionRow>> {
    if sid.is_empty() || sid.len() > 200 || sid.chars().any(char::is_control) {
        return Err(Fail::bad("the session ID is not valid"));
    }
    let (pid, pid_at) = match s.pid {
        Some(p) => (Some(p.pid), Some(p.started_at)),
        None => (None, None),
    };
    match store::session(c, sid)? {
        None if !s.create => return Ok(None),
        None => {
            c.execute(
                "INSERT INTO session(id, harness, state, pid, pid_started_at, cwd, started_at, last_seen_at) \
                 VALUES(?1,?2,'active',?3,?4,?5,?6,?6)",
                rusqlite::params![sid, harness_of(sid), pid, pid_at, s.cwd, now],
            )?;
            store::event(c, now, None, Some(sid), "session.started", json!({ "cwd": s.cwd, "via": s.via }))?;
            mark(home, sid);
        }
        Some(_) if !s.activate => {
            c.execute(
                "UPDATE session SET last_seen_at=?2, pid=COALESCE(?3, pid), pid_started_at=COALESCE(?4, pid_started_at) WHERE id=?1",
                rusqlite::params![sid, now, pid, pid_at],
            )?;
        }
        Some(row) => {
            c.execute(
                "UPDATE session SET state='active', ended_at=NULL, last_seen_at=?2, cwd=COALESCE(?3, cwd), \
                 pid=COALESCE(?4, pid), pid_started_at=COALESCE(?5, pid_started_at) WHERE id=?1",
                rusqlite::params![sid, now, s.cwd, pid, pid_at],
            )?;
            if row.state == "ended" {
                store::event(c, now, None, Some(sid), "session.resumed", json!({ "cwd": s.cwd, "via": s.via }))?;
                readopt(c, sid, now)?;
                mark(home, sid);
            }
        }
    }
    Ok(store::session(c, sid)?)
}

/// Orphaned leases of a session that is back become active again.
pub fn readopt(c: &Connection, sid: &str, now: i64) -> R<()> {
    for l in store::leases_session(c, sid)? {
        if l.state == tcas_core::lease::LeaseState::Orphaned {
            c.execute(
                "UPDATE lease SET state='active' WHERE workspace=?1 AND root=?2 AND path_key=?3",
                rusqlite::params![l.workspace, l.root, l.path_key],
            )?;
            store::event(c, now, Some(&l.workspace), Some(sid), "lease.readopted", json!({ "root": l.root, "path": l.path }))?;
        }
    }
    Ok(())
}

/// The marker `tcas hook` looks for to keep reporting a session whose
/// directory is outside every workspace (harness.md section 3).
pub fn mark(home: &Home, sid: &str) {
    let p = home.session_marker(sid);
    if !p.exists() {
        let _ = std::fs::create_dir_all(home.sessions_dir());
        let _ = std::fs::write(&p, "");
    }
}

pub fn unmark(home: &Home, sid: &str) {
    let _ = std::fs::remove_file(home.session_marker(sid));
}
