//! Connecting to tcasd, starting it when it is not running
//! (daemon-and-cli.md section 2).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tcas_core::proto::{ProcId, Request, Response, VERSION};
use tcas_sys::home::Home;

pub struct Client {
    pub home: Home,
    pub cwd: PathBuf,
    pub session: Option<String>,
    pub ancestors: Vec<ProcId>,
}

/// The session this command runs as (harness.md section 2). Claude Code's
/// own variable holds the bare ID; tcas IDs carry the harness in front, as
/// `TCAS_SESSION` written by the hook does.
pub fn session_from_env() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    var("TCAS_SESSION")
        .or_else(|| var("CLAUDE_CODE_SESSION_ID").map(|v| format!("claude:{v}")))
        .or_else(|| var("ANTIGRAVITY_CONVERSATION_ID").map(|v| format!("agy:{v}")))
}

impl Client {
    pub fn from_env() -> Client {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Client { home: Home::from_env(), cwd, session: session_from_env(), ancestors: tcas_sys::proc::ancestors() }
    }

    pub fn request(&self, op: &str, args: Value) -> Request {
        Request {
            v: VERSION,
            op: op.into(),
            session: self.session.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            ancestors: self.ancestors.clone(),
            args,
        }
    }

    pub fn connect(&self) -> std::io::Result<UnixStream> {
        UnixStream::connect(self.home.socket())
    }

    /// Starts tcasd detached from this process: its own session, working
    /// directory `/`, output to the log, no session or git variables.
    pub fn spawn_daemon(&self) -> Result<(), String> {
        self.home.ensure().map_err(|e| format!("{}: {e}", self.home.dir.display()))?;
        let exe = std::env::current_exe().map_err(|e| format!("cannot find the tcas program: {e}"))?;
        let sibling = exe.with_file_name("tcasd");
        let mut c = if sibling.is_file() {
            Command::new(sibling)
        } else {
            let mut c = Command::new(&exe);
            c.args(["daemon", "run"]);
            c
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.log())
            .map_err(|e| format!("{}: {e}", self.home.log().display()))?;
        let log2 = log.try_clone().map_err(|e| e.to_string())?;
        c.current_dir("/").stdin(Stdio::null()).stdout(log).stderr(log2);
        c.env_remove("TCAS_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("ANTIGRAVITY_AGENT");
        for v in tcasd::git::LOCAL_VARS {
            c.env_remove(v);
        }
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = c.spawn().map_err(|e| format!("cannot start tcasd: {e}"))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// A connection, starting tcasd if needed and waiting up to `wait`.
    pub fn ensure(&self, wait: Duration) -> Result<UnixStream, String> {
        if let Ok(s) = self.connect() {
            return Ok(s);
        }
        self.spawn_daemon()?;
        let deadline = Instant::now() + wait;
        loop {
            if let Ok(s) = self.connect() {
                return Ok(s);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "tcasd is not available (could not start it; see {})",
                    self.home.log().display()
                ));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    pub fn send(stream: &mut UnixStream, req: &Request) -> Result<(), String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|e| format!("tcasd: {e}"))
    }

    /// Reads the one-line response. `Ok(None)` when tcasd closed the
    /// connection without answering.
    pub fn receive(stream: &UnixStream) -> Result<Option<Response>, String> {
        let mut line = String::new();
        match BufReader::new(stream).read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => serde_json::from_str(&line).map(Some).map_err(|e| format!("tcasd sent an unreadable answer: {e}")),
            Err(e) => Err(format!("tcasd: {e}")),
        }
    }

    pub fn call(&self, op: &str, args: Value) -> Result<Response, String> {
        let mut s = self.ensure(Duration::from_secs(5))?;
        s.set_read_timeout(Some(Duration::from_secs(120))).ok();
        Self::send(&mut s, &self.request(op, args))?;
        Self::receive(&s)?.ok_or_else(|| "tcasd closed the connection without answering".to_string())
    }
}
