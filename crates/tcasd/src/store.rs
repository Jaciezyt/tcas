//! SQLite storage (daemon-and-cli.md section 3).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use tcas_core::lease::{LeaseState, Source};
use tcas_core::proto::EventView;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS workspace(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, mode TEXT, opened_by TEXT NOT NULL,
  created_at INTEGER NOT NULL, git_hooks INTEGER);
CREATE TABLE IF NOT EXISTS root(
  workspace TEXT NOT NULL, name TEXT NOT NULL, ord INTEGER NOT NULL, path TEXT NOT NULL UNIQUE,
  case_insensitive INTEGER NOT NULL, added_by TEXT NOT NULL, added_at INTEGER NOT NULL,
  PRIMARY KEY(workspace, name));
CREATE TABLE IF NOT EXISTS repo(
  workspace TEXT NOT NULL, root TEXT NOT NULL, dir TEXT NOT NULL, kind TEXT NOT NULL,
  common_dir TEXT, last_branch TEXT, branch_changed_at INTEGER, seen_at INTEGER NOT NULL,
  PRIMARY KEY(workspace, root, dir));
CREATE TABLE IF NOT EXISTS session(
  id TEXT PRIMARY KEY, harness TEXT NOT NULL, state TEXT NOT NULL, pid INTEGER,
  pid_started_at INTEGER, cwd TEXT, title TEXT, brief TEXT, started_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL, ended_at INTEGER);
CREATE TABLE IF NOT EXISTS lease(
  workspace TEXT NOT NULL, root TEXT NOT NULL, path TEXT NOT NULL, path_key TEXT NOT NULL,
  is_prefix INTEGER NOT NULL, repo TEXT, session TEXT NOT NULL, state TEXT NOT NULL,
  source TEXT NOT NULL, brief TEXT, acquired_at INTEGER NOT NULL, touched_at INTEGER NOT NULL,
  committed_at INTEGER,
  PRIMARY KEY(workspace, root, path_key));
CREATE INDEX IF NOT EXISTS lease_session ON lease(session);
CREATE TABLE IF NOT EXISTS open_call(
  session TEXT NOT NULL, agent TEXT, call_id TEXT NOT NULL, kind TEXT NOT NULL, paths TEXT,
  background_id TEXT, opened_at INTEGER NOT NULL,
  PRIMARY KEY(session, call_id));
CREATE TABLE IF NOT EXISTS notice(
  id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL, kind TEXT NOT NULL,
  body TEXT NOT NULL, created_at INTEGER NOT NULL, delivered_at INTEGER);
CREATE INDEX IF NOT EXISTS notice_session ON notice(session, delivered_at);
CREATE TABLE IF NOT EXISTS interest(
  session TEXT NOT NULL, workspace TEXT NOT NULL, root TEXT NOT NULL, path TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY(session, workspace, root, path, kind));
CREATE TABLE IF NOT EXISTS unattributed(
  workspace TEXT NOT NULL, root TEXT NOT NULL, path TEXT NOT NULL, seen_at INTEGER NOT NULL,
  PRIMARY KEY(workspace, root, path));
CREATE TABLE IF NOT EXISTS outside_edit(
  workspace TEXT NOT NULL, path TEXT NOT NULL, session TEXT NOT NULL, edits INTEGER NOT NULL,
  first_at INTEGER NOT NULL, last_at INTEGER NOT NULL,
  PRIMARY KEY(workspace, path, session));
CREATE TABLE IF NOT EXISTS event(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, workspace TEXT, session TEXT,
  agent TEXT, type TEXT NOT NULL, payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS commit_seen(
  hash TEXT NOT NULL, workspace TEXT NOT NULL, root TEXT NOT NULL, repo TEXT NOT NULL,
  by TEXT, at INTEGER NOT NULL, result TEXT NOT NULL,
  PRIMARY KEY(workspace, root, repo, hash));
"#;

/// Columns added after the first schema, for databases created before them.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("workspace", "git_hooks", "INTEGER"),
    ("repo", "branch_changed_at", "INTEGER"),
];

fn add_missing_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (table, col, ty) in ADDED_COLUMNS {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let have: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?;
        if !have.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"))?;
        }
    }
    Ok(())
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    add_missing_columns(&conn)?;
    let v: Option<String> =
        conn.query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0)).optional()?;
    if v.is_none() {
        conn.execute("INSERT INTO meta(key, value) VALUES('schema', ?1)", [SCHEMA_VERSION.to_string()])?;
    }
    Ok(conn)
}

#[derive(Debug, Clone)]
pub struct WsRow {
    pub id: String,
    pub name: String,
    pub mode: Option<String>,
    pub opened_by: String,
    pub created_at: i64,
    /// Opened with `--no-git-hooks`.
    pub no_git_hooks: bool,
}

#[derive(Debug, Clone)]
pub struct RootRow {
    pub workspace: String,
    pub name: String,
    pub ord: i64,
    pub path: String,
    pub ci: bool,
    pub added_by: String,
    pub added_at: i64,
}

#[derive(Debug, Clone)]
pub struct RepoRow {
    pub workspace: String,
    pub root: String,
    pub dir: String,
    pub kind: String,
    pub common_dir: Option<String>,
    pub last_branch: Option<String>,
    pub branch_changed_at: Option<i64>,
    pub seen_at: i64,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub harness: String,
    pub state: String,
    pub pid: Option<i32>,
    pub pid_started_at: Option<i64>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub brief: Option<String>,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub workspace: String,
    pub root: String,
    pub path: String,
    pub path_key: String,
    pub is_prefix: bool,
    /// The checkout holding the path, relative to the root; `None` outside any checkout.
    pub repo: Option<String>,
    pub session: String,
    pub state: LeaseState,
    pub source: Source,
    pub brief: Option<String>,
    pub acquired_at: i64,
    pub touched_at: i64,
    pub committed_at: Option<i64>,
}

fn ws_row(r: &Row) -> rusqlite::Result<WsRow> {
    Ok(WsRow {
        id: r.get(0)?,
        name: r.get(1)?,
        mode: r.get(2)?,
        opened_by: r.get(3)?,
        created_at: r.get(4)?,
        no_git_hooks: r.get::<_, Option<i64>>(5)? == Some(0),
    })
}

pub fn workspaces(c: &Connection) -> rusqlite::Result<Vec<WsRow>> {
    let mut st = c.prepare(
        "SELECT id, name, mode, opened_by, created_at, git_hooks FROM workspace ORDER BY created_at, id",
    )?;
    st.query_map([], ws_row)?.collect()
}

fn root_row(r: &Row) -> rusqlite::Result<RootRow> {
    Ok(RootRow {
        workspace: r.get(0)?,
        name: r.get(1)?,
        ord: r.get(2)?,
        path: r.get(3)?,
        ci: r.get::<_, i64>(4)? != 0,
        added_by: r.get(5)?,
        added_at: r.get(6)?,
    })
}

pub fn roots(c: &Connection) -> rusqlite::Result<Vec<RootRow>> {
    let mut st = c.prepare(
        "SELECT workspace, name, ord, path, case_insensitive, added_by, added_at FROM root ORDER BY workspace, ord",
    )?;
    st.query_map([], root_row)?.collect()
}

pub fn insert_root(c: &Connection, r: &RootRow) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO root(workspace, name, ord, path, case_insensitive, added_by, added_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![r.workspace, r.name, r.ord, r.path, r.ci as i64, r.added_by, r.added_at],
    )?;
    Ok(())
}

pub fn repos(c: &Connection, ws: &str) -> rusqlite::Result<Vec<RepoRow>> {
    let mut st = c.prepare(
        "SELECT workspace, root, dir, kind, common_dir, last_branch, branch_changed_at, seen_at FROM repo WHERE workspace=?1 ORDER BY root, dir",
    )?;
    st.query_map([ws], |r| {
        Ok(RepoRow {
            workspace: r.get(0)?,
            root: r.get(1)?,
            dir: r.get(2)?,
            kind: r.get(3)?,
            common_dir: r.get(4)?,
            last_branch: r.get(5)?,
            branch_changed_at: r.get(6)?,
            seen_at: r.get(7)?,
        })
    })?
    .collect()
}

pub fn all_repos(c: &Connection) -> rusqlite::Result<Vec<RepoRow>> {
    let mut out = Vec::new();
    for w in workspaces(c)? {
        out.extend(repos(c, &w.id)?);
    }
    Ok(out)
}

const SESSION_COLS: &str =
    "id, harness, state, pid, pid_started_at, cwd, title, brief, started_at, last_seen_at, ended_at";

fn session_row(r: &Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        harness: r.get(1)?,
        state: r.get(2)?,
        pid: r.get(3)?,
        pid_started_at: r.get(4)?,
        cwd: r.get(5)?,
        title: r.get(6)?,
        brief: r.get(7)?,
        started_at: r.get(8)?,
        last_seen_at: r.get(9)?,
        ended_at: r.get(10)?,
    })
}

pub fn session(c: &Connection, id: &str) -> rusqlite::Result<Option<SessionRow>> {
    c.query_row(&format!("SELECT {SESSION_COLS} FROM session WHERE id=?1"), [id], session_row).optional()
}

pub fn sessions(c: &Connection) -> rusqlite::Result<Vec<SessionRow>> {
    let mut st = c.prepare(&format!("SELECT {SESSION_COLS} FROM session ORDER BY started_at, id"))?;
    st.query_map([], session_row)?.collect()
}

const LEASE_COLS: &str = "workspace, root, path, path_key, is_prefix, repo, session, state, source, brief, acquired_at, touched_at, committed_at";

fn lease_row(r: &Row) -> rusqlite::Result<LeaseRow> {
    let state: String = r.get(7)?;
    let source: String = r.get(8)?;
    Ok(LeaseRow {
        workspace: r.get(0)?,
        root: r.get(1)?,
        path: r.get(2)?,
        path_key: r.get(3)?,
        is_prefix: r.get::<_, i64>(4)? != 0,
        repo: r.get(5)?,
        session: r.get(6)?,
        state: LeaseState::parse(&state).unwrap_or(LeaseState::Active),
        source: Source::parse(&source).unwrap_or(Source::Cli),
        brief: r.get(9)?,
        acquired_at: r.get(10)?,
        touched_at: r.get(11)?,
        committed_at: r.get(12)?,
    })
}

pub fn leases_ws(c: &Connection, ws: &str) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut st = c.prepare(&format!("SELECT {LEASE_COLS} FROM lease WHERE workspace=?1 ORDER BY root, path_key"))?;
    st.query_map([ws], lease_row)?.collect()
}

pub fn leases_session(c: &Connection, s: &str) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut st = c.prepare(&format!("SELECT {LEASE_COLS} FROM lease WHERE session=?1 ORDER BY workspace, root, path_key"))?;
    st.query_map([s], lease_row)?.collect()
}

pub fn leases_all(c: &Connection) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut st = c.prepare(&format!("SELECT {LEASE_COLS} FROM lease ORDER BY workspace, root, path_key"))?;
    st.query_map([], lease_row)?.collect()
}

pub fn insert_lease(c: &Connection, l: &LeaseRow) -> rusqlite::Result<()> {
    c.execute(
        &format!("INSERT OR REPLACE INTO lease({LEASE_COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"),
        params![
            l.workspace,
            l.root,
            l.path,
            l.path_key,
            l.is_prefix as i64,
            l.repo,
            l.session,
            l.state.as_str(),
            l.source.as_str(),
            l.brief,
            l.acquired_at,
            l.touched_at,
            l.committed_at
        ],
    )?;
    Ok(())
}

pub fn delete_lease(c: &Connection, l: &LeaseRow) -> rusqlite::Result<()> {
    c.execute(
        "DELETE FROM lease WHERE workspace=?1 AND root=?2 AND path_key=?3",
        params![l.workspace, l.root, l.path_key],
    )?;
    Ok(())
}

pub fn event(
    c: &Connection,
    at: i64,
    ws: Option<&str>,
    session: Option<&str>,
    kind: &str,
    payload: Value,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO event(at, workspace, session, type, payload) VALUES(?1,?2,?3,?4,?5)",
        params![at, ws, session, kind, payload.to_string()],
    )?;
    Ok(())
}

pub fn notice(c: &Connection, at: i64, session: &str, kind: &str, body: Value) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO notice(session, kind, body, created_at) VALUES(?1,?2,?3,?4)",
        params![session, kind, body.to_string(), at],
    )?;
    Ok(())
}

pub struct EventFilter<'a> {
    pub workspace: Option<&'a str>,
    pub session: Option<&'a str>,
    pub after_seq: Option<i64>,
    pub limit: usize,
}

pub fn events(c: &Connection, f: &EventFilter) -> rusqlite::Result<Vec<EventView>> {
    let mut sql = String::from("SELECT seq, at, workspace, session, type, payload FROM event WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(w) = f.workspace {
        sql.push_str(" AND (workspace=? OR workspace IS NULL)");
        args.push(Box::new(w.to_string()));
    }
    if let Some(s) = f.session {
        sql.push_str(" AND session=?");
        args.push(Box::new(s.to_string()));
    }
    if let Some(a) = f.after_seq {
        sql.push_str(" AND seq>?");
        args.push(Box::new(a));
        sql.push_str(" ORDER BY seq ASC LIMIT ?");
    } else {
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
    }
    args.push(Box::new(f.limit as i64));
    let mut st = c.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut out: Vec<EventView> = st
        .query_map(refs.as_slice(), |r| {
            let payload: String = r.get(5)?;
            Ok(EventView {
                seq: r.get(0)?,
                at: r.get(1)?,
                workspace: r.get(2)?,
                session: r.get(3)?,
                kind: r.get(4)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    if f.after_seq.is_none() {
        out.reverse();
    }
    Ok(out)
}
