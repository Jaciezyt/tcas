//! The `tcas` command (daemon-and-cli.md section 5).

mod client;
mod commit;
mod doctor;
mod githook;
mod hook;
mod out;
mod plugin;

use std::time::Duration;

use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use client::Client;
use out::Out;
use tcas_core::human::{self, short_session};
use tcas_core::proto::*;
use tcas_sys::clock;
use tcas_sys::home::Home;

#[derive(Parser)]
#[command(name = "tcas", version, about = "Leases on files and directories for coding agents sharing a checkout")]
struct Cli {
    /// Print the result as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open, extend, close and list workspaces.
    #[command(subcommand)]
    Workspace(WsCmd),
    /// The current workspace: sessions, their leases, unattributed changes.
    Status {
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Request, wait for, release and inspect leases.
    #[command(subcommand)]
    Lease(LeaseCmd),
    /// List or end sessions.
    #[command(subcommand)]
    Session(SessionCmd),
    /// Directories tcas never manages.
    #[command(subcommand)]
    Exempt(ExemptCmd),
    /// The event log.
    Log {
        #[arg(short = 'n')]
        n: Option<usize>,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Events of every workspace.
        #[arg(long)]
        all: bool,
    },
    /// Start, stop or inspect tcasd.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Check installation, daemon, workspace, git and harness health.
    Doctor,
    /// Who tcasd takes this command to come from.
    Whoami,
    /// Harness plugins.
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// Commit only this session's changed files, one commit per checkout.
    Commit {
        #[arg(short = 'm', long = "message")]
        message: String,
        /// Files or directories; by default every file this session changed.
        paths: Vec<String>,
    },
    /// Install, remove or show tcas's git hooks in a checkout.
    #[command(name = "git-hooks", subcommand)]
    GitHooks(GitHooksCmd),
    /// Called by a harness hook with the event on stdin.
    #[command(hide = true)]
    Hook { harness: String },
    /// Called by git's hooks.
    #[command(name = "git-hook", hide = true)]
    GitHook {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum GitHooksCmd {
    /// Install them (user only).
    Install { repo: Option<String> },
    /// Remove them (user only).
    Remove { repo: Option<String> },
    /// Whether they are installed.
    Status { repo: Option<String> },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Generate and install the plugin for a harness (user only).
    Install { harness: String },
    /// Print the plugin directory for a harness (writing it if needed), for
    /// `claude --plugin-dir`.
    Path { harness: String },
}

#[derive(Subcommand)]
enum WsCmd {
    /// Register a workspace; each directory becomes one of its roots.
    Open {
        paths: Vec<String>,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        no_git_hooks: bool,
    },
    /// Add directories to the current workspace as roots.
    Add {
        #[arg(required = true)]
        dirs: Vec<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Remove a root from its workspace.
    Remove {
        dir: String,
        #[arg(long)]
        force: bool,
    },
    /// Unregister a workspace.
    Close {
        path: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// List workspaces.
    List,
    /// Change a setting: `set mode auto|strict`.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum LeaseCmd {
    /// Request files or directories, all or nothing.
    Acquire {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        brief: Option<String>,
    },
    /// Like acquire, but wait until everything is granted.
    Wait {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Seconds; default 1800.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        brief: Option<String>,
    },
    /// Release your leases (with --force: anyone's; user only).
    Release {
        paths: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
    },
    /// Leases: your own, or all of them for the user and with --all.
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Who holds each path.
    Who {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Give a session leases (user only).
    Grant {
        session: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        brief: Option<String>,
    },
    /// Move leases to another session (user only).
    Transfer {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        to: String,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List sessions.
    List {
        /// Include ended sessions without leases.
        #[arg(long)]
        all: bool,
    },
    /// End a session: clean leases are released, the rest are orphaned (user only).
    End { session: String },
}

#[derive(Subcommand)]
enum ExemptCmd {
    /// List exempt directories and where each comes from.
    List,
    /// Add a directory to the global exempt list (user only).
    Add { path: String },
    /// Remove a directory from the global exempt list (user only).
    Remove { path: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start tcasd in the background.
    Start,
    /// Stop tcasd (user only).
    Stop,
    /// Whether tcasd is running.
    Status,
    /// Run tcasd in the foreground.
    Run,
}

pub struct Ctx {
    c: Client,
    o: Out,
    json: bool,
}

pub fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

impl Ctx {
    pub fn fail(&self, code: &str, message: &str) -> i32 {
        if self.json {
            print_json(&json!({ "ok": false, "error": { "code": code, "message": message } }));
        } else {
            eprintln!("tcas: {message}");
        }
        code::exit_code(code)
    }

    fn answer(&self, r: Response) -> Result<Value, i32> {
        if r.ok {
            return Ok(r.result.unwrap_or(Value::Null));
        }
        let e = r.error.unwrap_or(ErrorBody { code: code::INTERNAL.into(), message: "tcasd gave no reason".into(), detail: None });
        Err(self.fail(&e.code, &e.message))
    }

    pub fn call(&self, op: &str, args: Value) -> Result<Value, i32> {
        match self.c.call(op, args) {
            Ok(r) => self.answer(r),
            Err(m) => Err(self.fail("unavailable", &m)),
        }
    }

    /// Prints a result: JSON as is, or through `human`. The exit code in
    /// JSON mode comes from `code`.
    fn emit_with<T: DeserializeOwned>(
        &self,
        v: Value,
        human: impl FnOnce(&Out, &T) -> i32,
        code: impl FnOnce(&T) -> i32,
    ) -> i32 {
        let t: T = match serde_json::from_value(v.clone()) {
            Ok(t) => t,
            Err(e) => return self.fail(code::INTERNAL, &format!("tcasd sent an unexpected answer: {e}")),
        };
        if self.json {
            print_json(&v);
            code(&t)
        } else {
            human(&self.o, &t)
        }
    }

    fn emit<T: DeserializeOwned>(&self, v: Value, human: impl FnOnce(&Out, &T) -> i32) -> i32 {
        self.emit_with(v, human, |_| 0)
    }

    /// Prints a plain JSON result, or a line made from it.
    fn emit_plain(&self, v: Value, human: impl FnOnce(&Value)) -> i32 {
        if self.json { print_json(&v) } else { human(&v) }
        0
    }
}

fn main() {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Daemon(DaemonCmd::Run) => {
            if let Err(e) = tcasd::server::run(Home::from_env()) {
                eprintln!("tcasd: {e}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::Hook { harness } => std::process::exit(hook::run(harness)),
        Cmd::GitHook { name, args } => std::process::exit(githook::run(name, args)),
        Cmd::Plugin(PluginCmd::Path { harness }) => std::process::exit(plugin::path(harness, cli.json)),
        _ => {}
    }
    let c = Client::from_env();
    let o = Out::new(&c.cwd);
    let ctx = Ctx { c, o, json: cli.json };
    let code = match run(&ctx, cli.cmd) {
        Ok(code) | Err(code) => code,
    };
    std::process::exit(code);
}

fn run(x: &Ctx, cmd: Cmd) -> Result<i32, i32> {
    Ok(match cmd {
        Cmd::Workspace(w) => workspace(x, w)?,
        Cmd::Status { workspace } => {
            let v = x.call("status", json!({ "workspace": workspace }))?;
            x.emit(v, out::status)
        }
        Cmd::Lease(l) => lease(x, l)?,
        Cmd::Session(s) => session(x, s)?,
        Cmd::Exempt(e) => exempt(x, e)?,
        Cmd::Log { n, follow, session, path, workspace, all } => {
            let base = json!({ "n": n, "session": session, "path": path, "workspace": workspace, "all": all });
            log(x, base, follow)?
        }
        Cmd::Daemon(d) => daemon(x, d)?,
        Cmd::Doctor => doctor::run(x),
        Cmd::Hook { .. } | Cmd::GitHook { .. } | Cmd::Plugin(PluginCmd::Path { .. }) => unreachable!("handled in main"),
        Cmd::Plugin(PluginCmd::Install { harness }) => plugin::install(x, &harness)?,
        Cmd::Commit { message, paths } => commit::run(x, &message, paths)?,
        Cmd::GitHooks(g) => {
            let (op, repo) = match g {
                GitHooksCmd::Install { repo } => ("git_hooks.install", repo),
                GitHooksCmd::Remove { repo } => ("git_hooks.remove", repo),
                GitHooksCmd::Status { repo } => ("git_hooks.status", repo),
            };
            let repo = repo.map(|r| {
                let p = std::path::Path::new(&r);
                if p.is_absolute() { r } else { x.c.cwd.join(p).to_string_lossy().into_owned() }
            });
            let v = x.call(op, json!({ "repo": repo }))?;
            x.emit_plain(v, |v| {
                if v.get("supported").and_then(Value::as_bool) == Some(false) {
                    println!("this git does not run hooks defined in its configuration; tcas's git hooks are not used.");
                }
                for r in v["repos"].as_array().into_iter().flatten() {
                    let repo = x.o.tilde(r["repo"].as_str().unwrap_or("?"));
                    let state = match (op, r["installed"].as_bool(), r["changed"].as_bool()) {
                        ("git_hooks.install", _, Some(true)) => "installed",
                        ("git_hooks.install", _, _) => "already installed",
                        ("git_hooks.remove", _, Some(true)) => "removed",
                        ("git_hooks.remove", _, _) => "not installed",
                        (_, Some(true), _) => "installed",
                        _ => "not installed",
                    };
                    println!("{repo}: {state}");
                }
            })
        }
        Cmd::Whoami => {
            let v = x.call("whoami", json!({}))?;
            x.emit_plain(v, |v| {
                let who = v["caller"].as_str().unwrap_or("?");
                if v["user"].as_bool() == Some(true) {
                    println!("user (no session)");
                } else {
                    println!("session {who}");
                }
            })
        }
    })
}

fn workspace(x: &Ctx, w: WsCmd) -> Result<i32, i32> {
    Ok(match w {
        WsCmd::Open { paths, strict, no_git_hooks } => {
            let v = x.call("workspace.open", json!({ "paths": paths, "strict": strict, "no_git_hooks": no_git_hooks }))?;
            let r: OpenResult = serde_json::from_value(v.clone())
                .map_err(|e| x.fail(code::INTERNAL, &format!("tcasd sent an unexpected answer: {e}")))?;
            let code = x.emit(v, |o, r: &OpenResult| out::opened(o, r, "open"));
            if !x.json && !r.already_open && x.c.session.is_none() && r.workspace.opened_by == "user" {
                plugin::warn_if_missing(&x.c.home);
            }
            code
        }
        WsCmd::Add { dirs, workspace } => {
            let v = x.call("workspace.add", json!({ "paths": dirs, "workspace": workspace }))?;
            x.emit(v, |o, r: &OpenResult| out::opened(o, r, "add"))
        }
        WsCmd::Remove { dir, force } => {
            let v = x.call("workspace.remove", json!({ "path": dir, "force": force }))?;
            x.emit_plain(v, |v| {
                println!(
                    "removed root {} ({}) from workspace \"{}\".",
                    v["removed"].as_str().unwrap_or("?"),
                    x.o.tilde(v["path"].as_str().unwrap_or("")),
                    v["workspace"].as_str().unwrap_or("?")
                )
            })
        }
        WsCmd::Close { path, workspace, force } => {
            let v = x.call("workspace.close", json!({ "path": path, "workspace": workspace, "force": force }))?;
            x.emit_plain(v, |v| {
                let n = v["leases_dropped"].as_u64().unwrap_or(0) as usize;
                let dropped = if n > 0 { format!("; {} dropped", human::count(n, "lease", "leases")) } else { String::new() };
                println!("closed workspace \"{}\"{dropped}.", v["closed"].as_str().unwrap_or("?"));
            })
        }
        WsCmd::List => {
            let v = x.call("workspace.list", json!({}))?;
            x.emit(v, |o, r: &Vec<WorkspaceView>| out::workspace_list(o, r))
        }
        WsCmd::Set { key, value, workspace } => {
            let v = x.call("workspace.set", json!({ "key": key, "value": value, "workspace": workspace }))?;
            x.emit_plain(v, |v| {
                println!(
                    "workspace \"{}\" is now in mode {}.",
                    v["workspace"].as_str().unwrap_or("?"),
                    v["mode"].as_str().unwrap_or("?")
                )
            })
        }
    })
}

fn lease(x: &Ctx, l: LeaseCmd) -> Result<i32, i32> {
    Ok(match l {
        LeaseCmd::Acquire { paths, brief } => {
            let v = x.call("lease.acquire", json!({ "paths": paths, "brief": brief }))?;
            x.emit_with(v, |o, r: &AcquireResult| out::acquire(o, r, None), |r| r.outcome.exit_code())
        }
        LeaseCmd::Wait { paths, timeout, brief } => wait(x, paths, timeout, brief)?,
        LeaseCmd::Release { paths, all, force } => {
            let v = x.call("lease.release", json!({ "paths": paths, "all": all, "force": force }))?;
            x.emit(v, |o, r: &ReleaseResult| out::release(o, r, force))
        }
        LeaseCmd::List { all, workspace } => {
            let v = x.call("lease.list", json!({ "all": all, "workspace": workspace }))?;
            let show = all || x.c.session.is_none();
            x.emit(v, |o, r: &Vec<LeaseView>| out::lease_list(o, r, show))
        }
        LeaseCmd::Who { paths } => {
            let v = x.call("lease.who", json!({ "paths": paths }))?;
            x.emit(v, |o, r: &Vec<WhoEntry>| out::who(o, r))
        }
        LeaseCmd::Grant { session, paths, brief } => {
            let v = x.call("lease.grant", json!({ "session": session, "paths": paths, "brief": brief }))?;
            x.emit_with(v, |o, r: &AcquireResult| out::acquire(o, r, Some(&r.session)), |r| r.outcome.exit_code())
        }
        LeaseCmd::Transfer { paths, to } => {
            let v = x.call("lease.transfer", json!({ "paths": paths, "to": to }))?;
            x.emit_plain(v, |v| {
                let to = short_session(v["to"].as_str().unwrap_or("?"));
                let moved: Vec<String> = v["moved"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        format!(
                            "{} (from {})",
                            m["path"]["display"].as_str().unwrap_or("?"),
                            short_session(m["from"].as_str().unwrap_or("?"))
                        )
                    })
                    .collect();
                if !moved.is_empty() {
                    println!("transferred to {to}: {}", moved.join(", "));
                    println!("Both sessions will be told.");
                }
                let not_held: Vec<&str> = v["not_held"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if !not_held.is_empty() {
                    println!("not held by anyone: {}", not_held.join(", "));
                }
                let unmanaged: Vec<Unmanaged> = serde_json::from_value(v["unmanaged"].clone()).unwrap_or_default();
                out::print_unmanaged(&x.o, &unmanaged);
                if moved.is_empty() && not_held.is_empty() && unmanaged.is_empty() {
                    println!("nothing to transfer.");
                }
            })
        }
    })
}

/// `lease wait`: one connection held open until the wait ends. When tcasd
/// goes away it is started again and the wait registered again, keeping
/// the original start (daemon-and-cli.md section 2).
fn wait(x: &Ctx, paths: Vec<String>, timeout: Option<u64>, brief: Option<String>) -> Result<i32, i32> {
    let started = clock::now_ms();
    let mut resumed = false;
    let mut lost = 0;
    loop {
        let mut args = json!({ "paths": paths, "brief": brief, "timeout": timeout });
        if resumed {
            args["started_at"] = json!(started);
        }
        let mut s = x.c.ensure(Duration::from_secs(5)).map_err(|m| x.fail("unavailable", &m))?;
        let sent = Client::send(&mut s, &x.c.request("lease.wait", args));
        let got = match sent {
            Ok(()) => Client::receive(&s),
            Err(e) => Err(e),
        };
        match got {
            Ok(Some(r)) if !r.ok && r.error.as_ref().is_some_and(|e| e.code == "retry") => {}
            Ok(Some(r)) => {
                let v = x.answer(r)?;
                return Ok(x.emit_with(v, out::wait_result, |r: &AcquireResult| r.outcome.exit_code()));
            }
            // Closed without an answer, or the connection broke: tcasd is gone.
            Ok(None) | Err(_) => {
                lost += 1;
                if lost > 20 {
                    return Err(x.fail("unavailable", "lost tcasd too many times while waiting"));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        resumed = true;
    }
}

fn session(x: &Ctx, s: SessionCmd) -> Result<i32, i32> {
    Ok(match s {
        SessionCmd::List { all } => {
            let v = x.call("session.list", json!({ "all": all }))?;
            let user = x.c.session.is_none();
            x.emit(v, |o, r: &Vec<SessionView>| out::session_list(o, r, user))
        }
        SessionCmd::End { session } => {
            let v = x.call("session.end", json!({ "session": session }))?;
            x.emit_plain(v, |v| {
                let n = v["released"].as_u64().unwrap_or(0) as usize;
                println!(
                    "ended {}: {} released.",
                    v["session"].as_str().unwrap_or("?"),
                    human::count(n, "lease", "leases")
                );
                let orphaned: Vec<&str> = v["orphaned"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if !orphaned.is_empty() {
                    println!("orphaned (uncommitted changes remain): {}", orphaned.join(", "));
                    println!("  Commit or discard the changes, or: tcas lease transfer <path> --to <session>");
                }
            })
        }
    })
}

fn exempt(x: &Ctx, e: ExemptCmd) -> Result<i32, i32> {
    Ok(match e {
        ExemptCmd::List => {
            let v = x.call("exempt.list", json!({}))?;
            x.emit_plain(v, |v| {
                let entries: Vec<ExemptView> = serde_json::from_value(v["entries"].clone()).unwrap_or_default();
                let w = entries.iter().map(|e| e.written.chars().count() + 2).max().unwrap_or(0).max(20);
                for e in &entries {
                    let pad = " ".repeat(w.saturating_sub(e.written.chars().count()));
                    println!("{}{pad}{}", e.written, e.source);
                }
                for p in v["problems"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    println!("problem: {p}");
                }
                for c in v["conflicts"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    println!("warning: {c}");
                }
            })
        }
        ExemptCmd::Add { path } => {
            let v = x.call("exempt.add", json!({ "path": path }))?;
            x.emit_plain(v, |v| {
                let p = v["path"].as_str().unwrap_or("?");
                if v["changed"].as_bool() == Some(true) {
                    println!("exempt: {p}");
                } else {
                    println!("already exempt: {p}");
                }
            })
        }
        ExemptCmd::Remove { path } => {
            let v = x.call("exempt.remove", json!({ "path": path }))?;
            x.emit_plain(v, |v| println!("no longer exempt: {}", v["path"].as_str().unwrap_or("?")))
        }
    })
}

fn log(x: &Ctx, base: Value, follow: bool) -> Result<i32, i32> {
    let v = x.call("log", base.clone())?;
    let mut evs: Vec<EventView> = serde_json::from_value(v.clone()).unwrap_or_default();
    if x.json && !follow {
        print_json(&v);
        return Ok(0);
    }
    let show = |evs: &[EventView]| {
        if x.json {
            for e in evs {
                println!("{}", serde_json::to_string(e).unwrap_or_default());
            }
        } else {
            out::events(evs);
        }
    };
    show(&evs);
    if !follow {
        if evs.is_empty() {
            println!("no events.");
        }
        return Ok(0);
    }
    let mut last = evs.last().map(|e| e.seq).unwrap_or(0);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let mut args = base.clone();
        args["after_seq"] = json!(last);
        args["n"] = json!(500);
        let v = x.call("log", args)?;
        evs = serde_json::from_value(v).unwrap_or_default();
        if let Some(e) = evs.last() {
            last = e.seq;
        }
        show(&evs);
    }
}

fn daemon(x: &Ctx, d: DaemonCmd) -> Result<i32, i32> {
    let status = |s: std::os::unix::net::UnixStream| -> Result<DaemonStatus, i32> {
        let mut s = s;
        Client::send(&mut s, &x.c.request("daemon.status", json!({}))).map_err(|m| x.fail("unavailable", &m))?;
        let r = Client::receive(&s)
            .map_err(|m| x.fail("unavailable", &m))?
            .ok_or_else(|| x.fail("unavailable", "tcasd closed the connection without answering"))?;
        let v = x.answer(r)?;
        serde_json::from_value(v).map_err(|e| x.fail(code::INTERNAL, &e.to_string()))
    };
    Ok(match d {
        DaemonCmd::Run => unreachable!("handled in main"),
        DaemonCmd::Start => {
            let was = x.c.connect().is_ok();
            let s = x.c.ensure(Duration::from_secs(5)).map_err(|m| x.fail("unavailable", &m))?;
            let st = status(s)?;
            if x.json {
                print_json(&serde_json::to_value(&st).unwrap_or_default());
            } else if was {
                println!("tcasd is already running (pid {}).", st.pid);
            } else {
                println!("tcasd started (pid {}).", st.pid);
            }
            0
        }
        DaemonCmd::Status => {
            let Ok(s) = x.c.connect() else {
                if x.json {
                    print_json(&json!({ "running": false }));
                } else {
                    println!("tcasd is not running.");
                }
                return Ok(1);
            };
            let st = status(s)?;
            if x.json {
                print_json(&serde_json::to_value(&st).unwrap_or_default());
            } else {
                let now = clock::now_ms();
                println!("tcasd {} running, pid {}, up {}", st.version, st.pid, human::duration(now - st.started_at));
                println!("home        {}", x.o.tilde(&st.home));
                println!("workspaces  {}", st.workspaces);
                println!("sessions    {} active", st.sessions);
                println!("waits       {}", st.waits);
            }
            0
        }
        DaemonCmd::Stop => {
            let Ok(mut s) = x.c.connect() else {
                if !x.json {
                    println!("tcasd is not running.");
                }
                return Ok(0);
            };
            Client::send(&mut s, &x.c.request("daemon.stop", json!({}))).map_err(|m| x.fail("unavailable", &m))?;
            let r = Client::receive(&s)
                .map_err(|m| x.fail("unavailable", &m))?
                .ok_or_else(|| x.fail("unavailable", "tcasd closed the connection without answering"))?;
            let v = x.answer(r)?;
            // Wait until it is gone, so a following start finds the lock free.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && x.c.home.socket().exists() {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(Duration::from_millis(100));
            x.emit_plain(v, |v| println!("tcasd stopped (pid {}).", v["pid"]))
        }
    })
}
