//! `tcas hook <harness>`: translates a harness hook into the `hook`
//! operation and back (daemon-and-cli.md section 4, harness.md sections 3
//! and 7). What does not concern any workspace is decided here, from
//! `workspaces.json`, without connecting to tcasd.

mod claude;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tcas_core::abspath;
use tcas_core::config::WorkspaceConfig;
use tcas_core::exempt;
use tcas_core::proto::{HookRequest, HookResponse};
use tcas_sys::fsys;
use tcas_sys::home::Home;
use tcasd::wsjson::{RootEntry, WsEntry, WsJson};

use crate::client::Client;

/// How long `tcas hook` waits for tcasd to start (harness.md section 7).
const START_WAIT: Duration = Duration::from_secs(2);

pub fn run(harness: &str) -> i32 {
    if std::env::var_os("TCAS_DISABLE").is_some_and(|v| !v.is_empty()) {
        return 0;
    }
    let mut input = String::new();
    if std::io::stdin().take(16 << 20).read_to_string(&mut input).is_err() {
        eprintln!("tcas hook: cannot read the hook input");
        return 1;
    }
    match harness {
        "claude" => claude::run(&input),
        other => {
            eprintln!("tcas hook: unknown harness {other:?} (known: claude)");
            1
        }
    }
}

/// What `tcas hook` knows without tcasd.
pub struct Local {
    pub home: Home,
    /// `home` with symlinks resolved.
    home_real: String,
    pub wss: Vec<WsEntry>,
    pub exempt: Vec<exempt::ExemptDir>,
}

impl Local {
    pub fn load() -> Local {
        let home = Home::from_env();
        let home_real = fsys::to_str(&std::fs::canonicalize(&home.dir).unwrap_or_else(|_| home.dir.clone()));
        let parsed: Option<WsJson> =
            std::fs::read_to_string(home.workspaces_json()).ok().and_then(|t| serde_json::from_str(&t).ok());
        let (wss, exempt) = match parsed {
            Some(j) => (j.workspaces, j.exempt),
            None => (vec![], vec![]),
        };
        Local { home, home_real, wss, exempt }
    }

    pub fn has_workspaces(&self) -> bool {
        !self.wss.is_empty()
    }

    pub fn root_of(&self, abs: &str) -> Option<(&WsEntry, &RootEntry)> {
        self.wss.iter().find_map(|w| {
            w.roots.iter().find(|r| abspath::contains(&r.path, abs, r.case_insensitive)).map(|r| (w, r))
        })
    }

    pub fn is_exempt(&self, abs: &str) -> bool {
        exempt::find(&self.exempt, abs).is_some()
            || self.root_of(abs).is_some_and(|(w, _)| exempt::find(&w.exempt, abs).is_some())
    }

    /// In a workspace root and not exempt: tcasd decides about it.
    pub fn managed(&self, abs: &str) -> bool {
        self.root_of(abs).is_some() && !self.is_exempt(abs)
    }

    /// tcasd has a record of this session.
    pub fn marked(&self, sid: &str) -> bool {
        self.home.session_marker(sid).exists()
    }

    /// Paths only the user may change (harness.md section 4 step 1).
    pub fn protected(&self, abs: &str) -> Option<String> {
        let home = fsys::to_str(&self.home.dir);
        if abspath::contains(&home, abs, false) || abspath::contains(&self.home_real, abs, false) {
            return Some(format!(
                "tcas: {abs} is part of tcas's own configuration and data. Only the user can change it; tell the \
                 user what you need."
            ));
        }
        if let Some((_, r)) = self.root_of(abs) {
            let name = Path::new(abs).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is = if r.case_insensitive { name.eq_ignore_ascii_case(".tcas.toml") } else { name == ".tcas.toml" };
            if is {
                return Some(format!(
                    "tcas: {abs} is tcas's configuration for this workspace. Only the user can change it; tell the \
                     user what you need."
                ));
            }
        }
        None
    }

    /// The `.tcas.toml` of the workspace holding `dir`.
    pub fn ws_config(&self, dir: &str) -> Option<WorkspaceConfig> {
        let (w, _) = self.root_of(dir)?;
        let first = w.roots.first()?;
        let text = std::fs::read_to_string(Path::new(&first.path).join(".tcas.toml")).ok()?;
        tcas_core::config::parse_workspace(&text).ok()
    }
}

/// An absolute, normalized form of a path from the harness.
pub fn abs(cwd: &str, p: &str) -> String {
    fsys::to_str(&fsys::normalize(&fsys::absolute(Path::new(cwd), Path::new(p))))
}

/// The harness process: this process's parent (the plugin's forwarder
/// `exec`s, harness.md section 2). `TCAS_HOOK_PID` overrides it, for tests
/// and wrappers.
pub fn harness_pid() -> (i32, i64) {
    let pid = std::env::var("TCAS_HOOK_PID")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or_else(|| unsafe { libc::getppid() });
    match tcas_sys::proc::proc_id(pid) {
        Some(p) => (p.pid, p.started_at),
        None => (0, 0),
    }
}

/// Sends one event to tcasd, starting it if needed, all before `deadline`.
pub fn contact(local: &Local, h: &HookRequest, deadline: Instant) -> Result<HookResponse, String> {
    let client = Client { home: local.home.clone(), cwd: PathBuf::from(&h.cwd), session: None, ancestors: vec![] };
    let left = |d: Instant| d.saturating_duration_since(Instant::now()).max(Duration::from_millis(10));
    let mut s = client.ensure(START_WAIT.min(left(deadline)))?;
    s.set_write_timeout(Some(left(deadline))).ok();
    s.set_read_timeout(Some(left(deadline))).ok();
    let args = serde_json::to_value(h).map_err(|e| e.to_string())?;
    Client::send(&mut s, &client.request("hook", args))?;
    let r = Client::receive(&s)?.ok_or_else(|| "tcasd closed the connection without answering".to_string())?;
    if !r.ok {
        let e = r.error.map(|e| e.message).unwrap_or_else(|| "tcasd gave no reason".into());
        return Err(format!("tcasd: {e}"));
    }
    serde_json::from_value(r.result.unwrap_or_default()).map_err(|e| format!("tcasd sent an unexpected answer: {e}"))
}

/// The reason for refusing an edit when tcasd cannot be reached.
pub fn unavailable(why: &str) -> String {
    format!(
        "tcas: tcasd is not available ({why}), so changes to files in this workspace are refused until it is back. \
         Tell the user; they can run: tcas daemon start"
    )
}
