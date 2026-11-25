//! Helpers shared by the end-to-end tests: a temporary tcas home and
//! project directory, and the `tcas` program built by cargo.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub mod session;
#[allow(unused_imports)]
pub use session::*;

pub const TCAS: &str = env!("CARGO_BIN_EXE_tcas");

pub struct T {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub proj: PathBuf,
}

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Run {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.out).unwrap_or_else(|e| panic!("not JSON ({e}):\n{}\n{}", self.out, self.err))
    }
}

pub fn git(dir: &Path, args: &[&str]) {
    let r = user_git(dir, args, &[]);
    assert_eq!(r.code, 0, "git {args:?}:\n{}\n{}", r.out, r.err);
}

/// git run by the user in a terminal: no session variables (the tests
/// themselves may run inside one).
pub fn user_git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut c = Command::new("git");
    c.args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("TCAS_SESSION")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("ANTIGRAVITY_AGENT")
        .env_remove("TCAS_ALLOW")
        .stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

pub fn git_text(dir: &Path, args: &[&str]) -> String {
    let r = user_git(dir, args, &[]);
    assert_eq!(r.code, 0, "git {args:?}:\n{}\n{}", r.out, r.err);
    r.out
}

impl T {
    pub fn new() -> T {
        // /tmp keeps the socket path short; the built-in exempt list (which
        // includes the temp directories) is turned off in this home.
        let dir = tempfile::Builder::new().prefix("tcas-e2e.").tempdir_in("/tmp").unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let proj = root.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(home.join("config.toml"), "[exempt]\nbuiltin = false\n").unwrap();
        T { dir, root, home, proj }
    }

    /// A repository at `proj/<name>` with the given files committed.
    pub fn repo(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = self.proj.join(name);
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q", "-b", "main"]);
        git(&d, &["config", "user.name", "t"]);
        git(&d, &["config", "user.email", "t@example.com"]);
        git(&d, &["config", "commit.gpgsign", "false"]);
        for (p, text) in files {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-qm", "init"]);
        d
    }

    pub fn cmd(&self, session: Option<&str>, args: &[&str]) -> Command {
        let mut c = Command::new(TCAS);
        c.args(args)
            .current_dir(&self.proj)
            .env("TCAS_HOME", &self.home)
            .env_remove("TCAS_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("ANTIGRAVITY_AGENT");
        if let Some(s) = session {
            c.env("TCAS_SESSION", s);
        }
        c
    }

    pub fn run(&self, session: Option<&str>, args: &[&str]) -> Run {
        let o: Output = self.cmd(session, args).stdin(Stdio::null()).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    pub fn ok(&self, session: Option<&str>, args: &[&str]) -> Run {
        let r = self.run(session, args);
        assert_eq!(r.code, 0, "tcas {args:?} as {session:?}:\n{}\n{}", r.out, r.err);
        r
    }

    pub fn spawn(&self, session: Option<&str>, args: &[&str]) -> Child {
        self.cmd(session, args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }

    pub fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.home.join("tcasd.pid")).ok()?.trim().parse().ok()
    }

    pub fn daemon_waits(&self) -> u64 {
        self.ok(None, &["--json", "daemon", "status"]).json()["waits"].as_u64().unwrap_or(0)
    }

    pub fn until(&self, what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let pid = self.pid();
        let _ = self.cmd(None, &["daemon", "stop"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        if let Some(pid) = pid
            && alive(pid)
        {
            kill(pid);
        }
    }
}

pub fn alive(pid: i32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub fn kill(pid: i32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn finish(child: Child) -> Run {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child = child;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child did not finish");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let o = child.wait_with_output().unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

pub fn two_repos(t: &T) {
    for name in ["api", "web"] {
        t.repo(
            name,
            &[
                ("src/auth/session.ts", "a\n"),
                ("src/auth/redirect.ts", "b\n"),
                ("src/app.ts", "c\n"),
                (".gitignore", "build/\nvendor/\n"),
            ],
        );
    }
    t.ok(None, &["workspace", "open"]);
}

