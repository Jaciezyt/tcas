//! End-to-end checks for milestone 2 (plan.md section 3): Claude Code hook
//! input piped into `tcas hook claude`, against a real tcasd. Each session
//! has a `sleep` process standing in for its harness process.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::*;

/// A Claude Code session as its hooks see it.
struct S<'a> {
    t: &'a T,
    id: String,
    cwd: PathBuf,
    proc: Child,
    calls: std::cell::Cell<u32>,
}

impl Drop for S<'_> {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

/// What a hook printed: nothing, a decision, or context.
struct Out {
    code: i32,
    v: Value,
    err: String,
}

impl Out {
    fn denied(&self) -> bool {
        self.v["hookSpecificOutput"]["permissionDecision"] == "deny"
    }
    fn allowed(&self) -> bool {
        self.v["hookSpecificOutput"]["permissionDecision"] == "allow"
    }
    fn reason(&self) -> String {
        self.v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap_or("").to_string()
    }
    fn context(&self) -> String {
        self.v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
    }
    fn silent(&self) -> bool {
        self.v.is_null()
    }
}

impl<'a> S<'a> {
    fn new(t: &'a T, id: &str) -> S<'a> {
        S::at(t, id, &t.proj)
    }

    fn at(t: &'a T, id: &str, cwd: &Path) -> S<'a> {
        let proc = Command::new("sleep").arg("600").stdin(Stdio::null()).spawn().unwrap();
        S { t, id: id.into(), cwd: cwd.to_path_buf(), proc, calls: 0.into() }
    }

    fn sid(&self) -> String {
        format!("claude:{}", self.id)
    }

    fn call_id(&self) -> String {
        self.calls.set(self.calls.get() + 1);
        format!("toolu_{}_{}", self.id, self.calls.get())
    }

    fn hook(&self, mut input: Value) -> Out {
        self.hook_env(&mut input, &[])
    }

    fn hook_env(&self, input: &mut Value, env: &[(&str, &str)]) -> Out {
        input["session_id"] = json!(self.id);
        input["transcript_path"] = json!("/dev/null");
        if input.get("cwd").is_none() {
            input["cwd"] = json!(self.cwd);
        }
        let mut c = Command::new(TCAS);
        c.args(["hook", "claude"])
            .current_dir(&self.cwd)
            .env("TCAS_HOME", &self.t.home)
            .env("TCAS_HOOK_PID", self.proc.id().to_string())
            .env_remove("TCAS_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("TCAS_DISABLE")
            .env_remove("CLAUDE_ENV_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let v = if out.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("hook output is not JSON ({e}): {out}"))
        };
        let r = Out { code: o.status.code().unwrap_or(-1), v, err: String::from_utf8_lossy(&o.stderr).into_owned() };
        assert_eq!(r.code, 0, "hook exited {}: {}", r.code, r.err);
        r
    }

    fn start(&self, source: &str) -> Out {
        self.hook(json!({ "hook_event_name": "SessionStart", "source": source }))
    }

    fn prompt(&self, text: &str) -> Out {
        self.hook(json!({ "hook_event_name": "UserPromptSubmit", "prompt": text }))
    }

    /// PreToolUse and, when allowed, the file change and PostToolUse.
    fn edit(&self, path: &Path, text: &str) -> Out {
        let id = self.call_id();
        let input = json!({ "file_path": path, "content": text });
        let pre = self.hook(json!({ "hook_event_name": "PreToolUse", "tool_name": "Write", "tool_input": input, "tool_use_id": id }));
        if pre.denied() {
            return pre;
        }
        std::fs::write(path, text).unwrap();
        let post = self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Write", "tool_input": input, "tool_use_id": id,
            "tool_response": { "type": "update", "filePath": path }
        }));
        Out { code: post.code, v: if post.silent() { pre.v } else { post.v }, err: post.err }
    }

    fn pre_edit(&self, path: &Path) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_use_id": id,
            "tool_input": { "file_path": path, "old_string": "a", "new_string": "b", "replace_all": false }
        }))
    }

    fn pre_bash(&self, command: &str, background: bool) -> Out {
        let id = self.call_id();
        let mut input = json!({ "command": command, "description": "test" });
        if background {
            input["run_in_background"] = json!(true);
        }
        self.hook(json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": input, "tool_use_id": id }))
    }

    /// Any tool finishing: where notices are delivered.
    fn post_other(&self) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": id,
            "tool_input": { "command": "true" }, "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
        }))
    }

    fn read(&self, path: &Path) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Read", "tool_use_id": id,
            "tool_input": { "file_path": path }, "tool_response": { "type": "text" }
        }))
    }

    fn stop(&self) -> Out {
        self.hook(json!({ "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": [] }))
    }

    fn end(&self, reason: &str) -> Out {
        self.hook(json!({ "hook_event_name": "SessionEnd", "reason": reason }))
    }

    /// `tcas …` run by this session from its Bash tool.
    fn tcas(&self, args: &[&str]) -> Run {
        let mut c = self.t.cmd(Some(&self.sid()), args);
        c.current_dir(&self.cwd);
        let o = c.stdin(Stdio::null()).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn spawn_tcas(&self, args: &[&str]) -> Child {
        let mut c = self.t.cmd(Some(&self.sid()), args);
        c.current_dir(&self.cwd);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
}

/// One repository at the workspace root, opened.
fn one_repo(t: &T) -> PathBuf {
    let r = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("src/auth/x.ts", "x\n"), ("src/auth/y.ts", "y\n"), ("src/util/z.ts", "z\n"), (".gitignore", "dist/\n")]);
    let out = t.cmd(None, &["workspace", "open"]).current_dir(&r).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    r
}

fn leases(t: &T) -> Vec<(String, String, String, String)> {
    let all = t.ok(None, &["--json", "lease", "list"]).json();
    let mut v: Vec<(String, String, String, String)> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["session"].as_str().unwrap().into(),
                l["path"]["display"].as_str().unwrap().into(),
                l["source"].as_str().unwrap().into(),
                l["state"].as_str().unwrap().into(),
            )
        })
        .collect();
    v.sort();
    v
}

fn events(t: &T) -> Vec<Value> {
    t.ok(None, &["--json", "log", "-n", "500", "--all"]).json().as_array().unwrap().clone()
}

fn commit(dir: &Path, paths: &[&str]) {
    let mut args = vec!["commit", "-qm", "x", "--"];
    args.extend(paths);
    git(dir, &args);
}

// ---------------------------------------------------------------------------

/// Cases 1, 2, 21: edits take leases; a dirty file held by another session
/// is refused with the holder, its brief line and a wait command.
#[test]
fn edits_take_leases_and_conflicts_are_refused() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    assert!(a.start("startup").silent());
    a.prompt("Fix the login redirect loop");
    b.start("startup");

    let r = a.edit(&app.join("a.txt"), "changed by A\n");
    assert!(r.silent(), "an allowed edit prints nothing: {}", r.v);
    assert_eq!(leases(&t), [("claude:aaaa1111-2222".into(), "a.txt".into(), "edit".into(), "active".into())]);

    let r = b.edit(&app.join("a.txt"), "changed by B\n");
    assert!(r.denied(), "{}", r.v);
    assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "changed by A\n");
    let why = r.reason();
    assert!(why.contains("tcas: a.txt is leased by another session (claude:aaaa…"), "{why}");
    assert!(why.contains("They have not written a brief."), "{why}");
    assert!(!why.contains("login redirect"), "the holder's title must not reach other sessions: {why}");
    assert!(why.contains("It has uncommitted work in progress. Do not modify it by any other means, including Bash."), "{why}");
    assert!(why.contains("run this as a background command (Bash tool, run_in_background):\n  tcas lease wait a.txt --brief"), "{why}");

    // With a brief, the line shows it.
    a.tcas(&["lease", "acquire", "a.txt", "--brief", "Fix the redirect loop"]);
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.reason().contains("Their brief: \"Fix the redirect loop\""), "{}", r.reason());
    assert!(r.reason().contains("If their brief already covers the change"), "{}", r.reason());

    // Paths outside every workspace, and git-excluded ones, are not checked.
    let other = t.root.join("elsewhere.txt");
    assert!(b.edit(&other, "x").silent());
    std::fs::create_dir_all(app.join("dist")).unwrap();
    assert!(a.edit(&app.join("dist/out.js"), "1").silent());
    assert!(b.edit(&app.join("dist/out.js"), "2").silent());
    assert_eq!(leases(&t).len(), 1);
}

/// Cases 5, 6, 7: a session that ends with a dirty file leaves an orphaned
/// lease; resuming takes it back; discarding the change frees it.
#[test]
fn orphans_resume_and_discard() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    a.end("prompt_input_exit");
    assert_eq!(leases(&t)[0].3, "orphaned");

    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.denied());
    let why = r.reason();
    assert!(why.contains("is held by a session that has ended (claude:aaaa…, ended"), "{why}");
    assert!(why.contains("They had not written a brief."), "{why}");
    assert!(why.contains("stays leased until the user commits or discards those changes"), "{why}");
    assert!(why.contains("It returns only after the user has dealt with it."), "{why}");

    // Resume: same session ID, new process.
    let a2 = S::at(&t, "aaaa1111-2222", &app);
    a2.start("resume");
    assert_eq!(leases(&t)[0].3, "active");
    assert!(a2.edit(&app.join("a.txt"), "A again\n").silent());
    a2.end("other");

    git(&app, &["checkout", "--", "a.txt"]);
    assert!(b.edit(&app.join("a.txt"), "B\n").silent());
    assert_eq!(leases(&t)[0].0, "claude:bbbb3333-4444");
}

/// Case 8: with tcasd unavailable, edits in a workspace are refused and
/// others are not.
#[test]
fn tcasd_unavailable_refuses_workspace_edits() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    t.ok(None, &["daemon", "stop"]);
    // tcasd cannot start: its home is read-only.
    let mode = |m: u32| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&t.home, std::fs::Permissions::from_mode(m)).unwrap();
    };
    mode(0o500);
    let started = Instant::now();
    let r = a.pre_edit(&app.join("a.txt"));
    let took = started.elapsed();
    let outside = a.edit(&t.root.join("outside.txt"), "x");
    mode(0o700);
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("tcasd is not available"), "{}", r.reason());
    assert!(r.reason().contains("tcas daemon start"), "{}", r.reason());
    assert!(!r.reason().contains("tcas lease wait"), "{}", r.reason());
    assert!(took < Duration::from_secs(7), "{took:?}");
    assert!(outside.silent());
}

/// Cases 22, 25: a background wait is granted when the holder commits;
/// the notice comes with the next hook, after a tool call or on the
/// wake-up prompt.
#[test]
fn background_wait_and_notice() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    assert!(b.pre_edit(&app.join("a.txt")).denied());

    let waiter = b.spawn_tcas(&["lease", "wait", "a.txt", "--timeout", "30", "--brief", "add a line"]);
    t.until("the wait to register", || t.daemon_waits() == 1);
    b.stop();
    commit(&app, &["a.txt"]);
    let r = finish(waiter);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(leases(&t).iter().any(|l| l.0 == "claude:bbbb3333-4444" && l.1 == "a.txt" && l.2 == "wait"));

    let r = b.prompt("<task-notification>\n<task-id>x</task-id>\n<status>completed</status>\n</task-notification>");
    assert!(r.context().starts_with("tcas: your wait is over"), "{}", r.v);
    assert!(r.context().contains("This session now holds: a.txt."), "{}", r.context());
    assert!(b.post_other().silent(), "delivered once");
    assert!(b.edit(&app.join("a.txt"), "B\n").silent());

    // The wake-up prompt is not the title.
    let st = t.ok(None, &["--json", "session", "list"]).json();
    let bs = st.as_array().unwrap().iter().find(|s| s["id"] == "claude:bbbb3333-4444").unwrap().clone();
    assert!(bs["title"].is_null(), "{bs}");
}

/// Case 23: a wait that times out leaves nothing behind and a notice.
#[test]
fn wait_timeout_notice() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    let r = b.tcas(&["lease", "wait", "a.txt", "--timeout", "1"]);
    assert_eq!(r.code, 4, "{}", r.out);
    assert_eq!(t.daemon_waits(), 0);
    assert!(!leases(&t).iter().any(|l| l.0.contains("bbbb")));
    let c = b.post_other().context();
    assert!(c.contains("your wait for a.txt timed out"), "{c}");
}

/// Case 26: waiting for an orphaned file; the user discards the change.
#[test]
fn wait_for_orphan_after_discard() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    a.end("other");
    let waiter = b.spawn_tcas(&["lease", "wait", "a.txt", "--timeout", "30"]);
    t.until("the wait to register", || t.daemon_waits() == 1);
    git(&app, &["checkout", "--", "a.txt"]);
    let started = Instant::now();
    let r = finish(waiter);
    assert_eq!(r.code, 0, "{}", r.out);
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// Case 28 and harness.md 5.5: tcas commands are approved, a foreground
/// wait is moved to the background, a wait inside a longer command is
/// refused, and other commands are left to the harness.
#[test]
fn tcas_commands_in_bash() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");

    let r = a.pre_bash("tcas lease wait a.txt --brief 'need it'", false);
    assert!(r.allowed(), "{}", r.v);
    let u = &r.v["hookSpecificOutput"]["updatedInput"];
    assert_eq!(u["run_in_background"], true, "{}", r.v);
    assert_eq!(u["command"], "tcas lease wait a.txt --brief 'need it'");
    assert_eq!(u["description"], "test", "the rest of the input is kept");

    let r = a.pre_bash("tcas lease wait a.txt", true);
    assert!(r.allowed());
    assert!(r.v["hookSpecificOutput"].get("updatedInput").is_none());

    for ok in ["tcas status", "tcas lease acquire src/ --brief x", "tcas --json lease list", "tcas lease who a.txt"] {
        assert!(a.pre_bash(ok, false).allowed(), "{ok}");
    }
    for left in ["tcas commit -m x", "tcas workspace open", "tcas workspace add ../x", "tcas lease grant B a.txt", "ls -la", "tcas status > out.txt"] {
        assert!(a.pre_bash(left, false).silent(), "{left}");
    }
    let r = a.pre_bash("tcas lease wait a.txt && echo done", false);
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("as a background command"), "{}", r.reason());
    assert!(a.pre_bash("tcas lease wait a.txt && echo done", true).silent());

    std::fs::write(app.join(".tcas.toml"), "[bash]\napprove_tcas = false\n").unwrap();
    assert!(a.pre_bash("tcas status", false).silent());
    assert!(a.pre_bash("tcas lease wait a.txt", false).denied());
}

/// harness.md section 4 step 1: tcas's own files and `.tcas.toml` are refused.
#[test]
fn config_files_are_refused() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    for p in [t.home.join("config.toml"), app.join(".tcas.toml")] {
        let r = a.pre_edit(&p);
        assert!(r.denied(), "{}", p.display());
        assert!(r.reason().contains("Only the user can change it"), "{}", r.reason());
    }
}

/// Cases 30, 31: an explicit lease left unused yields after the idle
/// limit; reading under it counts as using it.
#[test]
fn explicit_leases_expire_unless_used() {
    let t = T::new();
    let app = one_repo(&t);
    std::fs::write(app.join(".tcas.toml"), "[lease]\nexplicit_idle_secs = 2\n").unwrap();
    git(&app, &["add", ".tcas.toml"]);
    commit(&app, &[".tcas.toml"]);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.tcas(&["lease", "acquire", "src/", "--brief", "restructure src"]);

    // Used by reading: still held after the original limit.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(900));
        a.read(&app.join("src/auth/x.ts"));
    }
    assert!(b.pre_edit(&app.join("src/util/z.ts")).denied(), "reads keep it");

    std::thread::sleep(Duration::from_millis(2200));
    let r = b.edit(&app.join("src/util/z.ts"), "B\n");
    assert!(r.silent(), "{}", r.v);
    assert!(!leases(&t).iter().any(|l| l.0.contains("aaaa")));

    // A learns at its next hook, first.
    let c = a.prompt("next").context();
    assert!(c.starts_with("tcas: these leases of yours were given to other sessions:"), "{c}");
    assert!(c.contains("src/") && c.contains("not used for a long time"), "{c}");
}

/// Case 32, 33: a dirty file of an explicit lease never yields; once the
/// holder is inactive long enough it is stalled and the reason says so.
#[test]
fn dirty_files_stay_and_stall() {
    let t = T::new();
    let app = one_repo(&t);
    std::fs::write(app.join(".tcas.toml"), "[lease]\nexplicit_idle_secs = 1\nstalled_after_secs = 2\n").unwrap();
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.tcas(&["lease", "acquire", "a.txt"]);
    a.edit(&app.join("a.txt"), "A\n");
    a.stop();
    std::thread::sleep(Duration::from_millis(1300));
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.denied());
    assert!(!r.reason().contains("no activity"), "{}", r.reason());
    std::thread::sleep(Duration::from_millis(1200));
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.reason().contains("that session has had no activity for"), "{}", r.reason());
    assert!(r.reason().contains("Tell the user."), "{}", r.reason());
    let app_s = app.to_str().unwrap();
    let st = t.ok(None, &["--json", "status", "--workspace", app_s]).json();
    assert!(st["leases"][0]["stalled"].as_bool().unwrap());
    a.prompt("back");
    let st = t.ok(None, &["--json", "status", "--workspace", app_s]).json();
    assert!(!st["leases"][0]["stalled"].as_bool().unwrap());
}

/// Cases 34, 38: a directory lease blocks edits under it; when its session
/// ends, the directory goes and the dirty file stays as an orphan.
#[test]
fn directory_leases_and_session_end() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.tcas(&["lease", "acquire", "src/auth/", "--brief", "Rename getUser"]);
    for p in ["src/auth/x.ts", "src/auth/new.ts"] {
        let r = b.pre_edit(&app.join(p));
        assert!(r.denied(), "{p}");
        assert!(r.reason().contains(&format!("{p} is in src/auth/, which is leased by another session")), "{}", r.reason());
        assert!(r.reason().contains("Their brief: \"Rename getUser\""), "{}", r.reason());
        assert!(r.reason().contains(&format!("tcas lease wait {p} ")), "the wait names the file: {}", r.reason());
    }
    assert!(b.edit(&app.join("src/util/z.ts"), "B\n").silent());

    a.edit(&app.join("src/auth/x.ts"), "A\n");
    let mine: Vec<String> = leases(&t).into_iter().filter(|l| l.0.contains("aaaa")).map(|l| l.1).collect();
    assert_eq!(mine, ["src/auth/", "src/auth/x.ts"]);
    a.end("other");
    let left: Vec<(String, String)> = leases(&t).into_iter().filter(|l| l.0.contains("aaaa")).map(|l| (l.1, l.3)).collect();
    assert_eq!(left, [("src/auth/x.ts".to_string(), "orphaned".to_string())]);
    assert!(b.edit(&app.join("src/auth/y.ts"), "B\n").silent());
}

/// Case 41: one session edits in two repositories of one workspace.
#[test]
fn edits_across_repositories() {
    let t = T::new();
    two_repos(&t);
    let (a, b) = (S::new(&t, "aaaa1111-2222"), S::new(&t, "bbbb3333-4444"));
    a.start("startup");
    a.edit(&t.proj.join("web/src/app.ts"), "A\n");
    a.edit(&t.proj.join("api/src/app.ts"), "A\n");
    let mine: Vec<String> = leases(&t).into_iter().map(|l| l.1).collect();
    assert_eq!(mine, ["api/src/app.ts", "web/src/app.ts"]);
    let r = b.pre_edit(&t.proj.join("api/src/app.ts"));
    assert!(r.reason().contains("tcas lease wait api/src/app.ts"), "{}", r.reason());
}

/// Case 88: edits in a checkout outside the workspace are let through,
/// hinted once, and listed by `tcas status`.
#[test]
fn edits_outside_the_workspace() {
    let t = T::new();
    let app = one_repo(&t);
    let site = t.proj.join("app-website");
    std::fs::create_dir_all(&site).unwrap();
    git(&site, &["init", "-q", "-b", "main"]);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    let r = a.edit(&site.join("index.html"), "1");
    assert!(r.context().contains("tcas: you edited a file in ../app-website, which is outside this workspace."), "{}", r.v);
    assert!(r.context().contains("tcas workspace add ../app-website"), "{}", r.context());
    assert!(a.edit(&site.join("index.html"), "2").silent(), "only once");
    let st = t.ok(None, &["--json", "status", "--workspace", app.to_str().unwrap()]).json();
    assert_eq!(st["outside"][0]["edits"], 2, "{st}");
    let text = t.cmd(None, &["status"]).current_dir(&app).output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("tcas workspace add ../app-website"), "{text}");
    // A file outside any checkout is not counted.
    assert!(a.edit(&t.root.join("notes.txt"), "x").silent());
}

/// harness.md section 2: `/clear` gives the conversation a new ID in the
/// same process; the new session takes the old one's leases over.
#[test]
fn clear_hands_leases_to_the_new_session() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    a.tcas(&["lease", "acquire", "src/auth/"]);
    a.end("clear");
    // The same process, under a new session ID.
    let a2 = S::at(&t, "cccc5555-6666", &app);
    let pid = a.proc.id().to_string();
    let r = a2.hook_env(&mut json!({ "hook_event_name": "SessionStart", "source": "clear" }), &[("TCAS_HOOK_PID", &pid)]);
    assert!(r.context().contains("now holds its leases: a.txt, src/auth/"), "{}", r.v);
    let all = leases(&t);
    assert!(all.iter().all(|l| l.0 == "claude:cccc5555-6666" && l.3 == "active"), "{all:?}");
    let sessions = t.ok(None, &["--json", "session", "list", "--all"]).json();
    let old = sessions.as_array().unwrap().iter().find(|s| s["id"] == "claude:aaaa1111-2222").unwrap().clone();
    assert_eq!(old["state"], "ended");
}

/// harness.md section 3: a session outside every workspace costs nothing
/// and needs no tcasd; `TCAS_SESSION` is written for it all the same.
#[test]
fn outside_sessions_do_not_touch_tcasd() {
    let t = T::new();
    let elsewhere = t.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let a = S::at(&t, "aaaa1111-2222", &elsewhere);
    let env_file = t.root.join("env.sh");
    let r = a.hook_env(
        &mut json!({ "hook_event_name": "SessionStart", "source": "startup" }),
        &[("CLAUDE_ENV_FILE", env_file.to_str().unwrap())],
    );
    assert!(r.silent());
    assert_eq!(std::fs::read_to_string(&env_file).unwrap(), "export TCAS_SESSION=claude:aaaa1111-2222\n");
    a.prompt("hello");
    a.edit(&elsewhere.join("x.txt"), "x");
    a.stop();
    assert!(t.pid().is_none(), "tcasd was started");

    // TCAS_DISABLE turns the hook off.
    let app = one_repo(&t);
    let b = S::at(&t, "bbbb3333-4444", &app);
    let r = b.hook_env(
        &mut json!({ "hook_event_name": "PreToolUse", "tool_name": "Write", "tool_use_id": "x", "tool_input": { "file_path": app.join(".tcas.toml"), "content": "" } }),
        &[("TCAS_DISABLE", "1")],
    );
    assert!(r.silent());
}

/// An edit call in progress keeps even a clean implicit lease (leases.md
/// section 5 step 4); `Stop` closes the calls the turn left open. The
/// committed-lease rule (case 105) needs `post-commit`, in milestone 3.
#[test]
fn busy_holder_keeps_its_lease() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    // A's edit call is open (PreToolUse without PostToolUse yet): the clean
    // implicit lease does not yield.
    assert!(a.pre_edit(&app.join("a.txt")).silent());
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("They are changing it right now."), "{}", r.reason());
    // Stop closes it; the file is clean, so it yields.
    a.stop();
    assert!(b.pre_edit(&app.join("a.txt")).silent());
    let ev = events(&t);
    assert!(ev.iter().any(|e| e["type"] == "lease.released" && e["payload"]["reason"] == "yielded"), "{ev:?}");
}

/// The plugin directory and its forwarder.
#[test]
fn plugin_path_and_forwarder() {
    let t = T::new();
    let r = t.ok(None, &["plugin", "path", "claude"]);
    let dir = PathBuf::from(r.out.trim());
    assert_eq!(dir, t.home.join("plugins/claude"));
    let hooks: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("hooks/hooks.json")).unwrap()).unwrap();
    let pre = hooks["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(pre.iter().all(|m| m["hooks"][0]["onFailure"] == "block"));
    assert!(dir.join(".claude-plugin/plugin.json").is_file());

    // The forwarder finds this tcas through the path written into it.
    let o = Command::new(dir.join("bin/tcas"))
        .arg("--version")
        .env("TCAS_HOME", &t.home)
        .env_remove("TCAS_BIN")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("tcas "));

    // Without any tcas program: nothing to protect, so the hook lets go;
    // with a workspace, a PreToolUse is refused.
    let lonely = t.root.join("lonely");
    std::fs::create_dir_all(lonely.join("bin")).unwrap();
    std::fs::copy(include_path(), lonely.join("bin/tcas")).unwrap();
    let run = |home: &Path| {
        let mut c = Command::new("sh");
        c.arg(lonely.join("bin/tcas"))
            .args(["hook", "claude"])
            .env("TCAS_HOME", home)
            .env_remove("TCAS_BIN")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        ch.stdin.take().unwrap().write_all(br#"{"hook_event_name":"PreToolUse","tool_name":"Write"}"#).unwrap();
        String::from_utf8_lossy(&ch.wait_with_output().unwrap().stdout).into_owned()
    };
    let empty = t.root.join("empty-home");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(run(&empty), "");
    std::fs::write(empty.join("workspaces.json"), r#"{"version":1,"workspaces":[{"id":"x"}],"exempt":[]}"#).unwrap();
    assert!(run(&empty).contains("\"permissionDecision\":\"deny\""));
}

fn include_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/claude/bin/tcas")
}
