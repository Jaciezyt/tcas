//! A Claude Code session as its hooks see it: hook input piped into
//! `tcas hook claude` against a real tcasd, with a `sleep` process standing
//! in for the harness process.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use super::*;

/// A Claude Code session as its hooks see it.
pub struct S<'a> {
    pub t: &'a T,
    pub id: String,
    pub cwd: PathBuf,
    pub proc: Child,
    pub calls: std::cell::Cell<u32>,
}

impl Drop for S<'_> {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

/// What a hook printed: nothing, a decision, or context.
pub struct Out {
    pub code: i32,
    pub v: Value,
    pub err: String,
}

impl Out {
    pub fn denied(&self) -> bool {
        self.v["hookSpecificOutput"]["permissionDecision"] == "deny"
    }
    pub fn allowed(&self) -> bool {
        self.v["hookSpecificOutput"]["permissionDecision"] == "allow"
    }
    pub fn reason(&self) -> String {
        self.v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap_or("").to_string()
    }
    pub fn context(&self) -> String {
        self.v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
    }
    pub fn silent(&self) -> bool {
        self.v.is_null()
    }
}

impl<'a> S<'a> {
    pub fn new(t: &'a T, id: &str) -> S<'a> {
        S::at(t, id, &t.proj)
    }

    pub fn at(t: &'a T, id: &str, cwd: &Path) -> S<'a> {
        let proc = Command::new("sleep").arg("600").stdin(Stdio::null()).spawn().unwrap();
        S { t, id: id.into(), cwd: cwd.to_path_buf(), proc, calls: 0.into() }
    }

    pub fn sid(&self) -> String {
        format!("claude:{}", self.id)
    }

    pub fn call_id(&self) -> String {
        self.calls.set(self.calls.get() + 1);
        format!("toolu_{}_{}", self.id, self.calls.get())
    }

    pub fn hook(&self, mut input: Value) -> Out {
        self.hook_env(&mut input, &[])
    }

    pub fn hook_env(&self, input: &mut Value, env: &[(&str, &str)]) -> Out {
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

    pub fn start(&self, source: &str) -> Out {
        self.hook(json!({ "hook_event_name": "SessionStart", "source": source }))
    }

    pub fn prompt(&self, text: &str) -> Out {
        self.hook(json!({ "hook_event_name": "UserPromptSubmit", "prompt": text }))
    }

    /// PreToolUse and, when allowed, the file change and PostToolUse.
    pub fn edit(&self, path: &Path, text: &str) -> Out {
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

    pub fn pre_edit(&self, path: &Path) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_use_id": id,
            "tool_input": { "file_path": path, "old_string": "a", "new_string": "b", "replace_all": false }
        }))
    }

    pub fn pre_bash(&self, command: &str, background: bool) -> Out {
        let id = self.call_id();
        let mut input = json!({ "command": command, "description": "test" });
        if background {
            input["run_in_background"] = json!(true);
        }
        self.hook(json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": input, "tool_use_id": id }))
    }

    /// A Bash tool call: PreToolUse, the command itself (unless refused),
    /// PostToolUse. The command runs in the session's directory with its
    /// session variable, as Claude Code runs it.
    pub fn bash(&self, command: &str) -> Bash {
        let id = self.call_id();
        let input = json!({ "command": command, "description": "test" });
        let pre = self.hook(json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": input, "tool_use_id": id }));
        if pre.denied() {
            return Bash { pre, run: None, post: None };
        }
        let run = self.sh(command);
        let post = self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": id, "tool_input": input,
            "tool_response": { "stdout": run.out, "stderr": run.err, "interrupted": false }
        }));
        Bash { pre, run: Some(run), post: Some(post) }
    }

    /// A background Bash call: PreToolUse, the command started, and
    /// PostToolUse with its task ID. The command runs on.
    pub fn bash_bg(&self, command: &str, task: &str) -> (Out, Option<Child>) {
        let id = self.call_id();
        let input = json!({ "command": command, "description": "test", "run_in_background": true });
        let pre = self.hook(json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": input, "tool_use_id": id }));
        if pre.denied() {
            return (pre, None);
        }
        let child = self.sh_cmd(command).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let post = self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": id, "tool_input": input,
            "tool_response": { "backgroundTaskId": task, "stdout": "", "stderr": "", "interrupted": false }
        }));
        (post, Some(child))
    }

    /// Stop with these background tasks still running.
    pub fn stop_with(&self, running: &[&str]) -> Out {
        let tasks: Vec<Value> = running.iter().map(|id| json!({ "id": id, "status": "running" })).collect();
        self.hook(json!({ "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": tasks }))
    }

    fn sh_cmd(&self, command: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", command])
            .current_dir(&self.cwd)
            .env("TCAS_HOME", &self.t.home)
            .env("TCAS_SESSION", self.sid())
            .env("PATH", path_with_tcas())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("TCAS_ALLOW")
            .stdin(Stdio::null());
        c
    }

    /// Runs a command as this session's Bash tool would, without hooks.
    pub fn sh(&self, command: &str) -> Run {
        let o = self.sh_cmd(command).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// Any tool finishing: where notices are delivered.
    pub fn post_other(&self) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": id,
            "tool_input": { "command": "true" }, "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
        }))
    }

    pub fn read(&self, path: &Path) -> Out {
        let id = self.call_id();
        self.hook(json!({
            "hook_event_name": "PostToolUse", "tool_name": "Read", "tool_use_id": id,
            "tool_input": { "file_path": path }, "tool_response": { "type": "text" }
        }))
    }

    pub fn stop(&self) -> Out {
        self.hook(json!({ "hook_event_name": "Stop", "stop_hook_active": false, "background_tasks": [] }))
    }

    pub fn end(&self, reason: &str) -> Out {
        self.hook(json!({ "hook_event_name": "SessionEnd", "reason": reason }))
    }

    /// `tcas …` run by this session from its Bash tool.
    pub fn tcas(&self, args: &[&str]) -> Run {
        let mut c = self.t.cmd(Some(&self.sid()), args);
        c.current_dir(&self.cwd);
        let o = c.stdin(Stdio::null()).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    pub fn spawn_tcas(&self, args: &[&str]) -> Child {
        let mut c = self.t.cmd(Some(&self.sid()), args);
        c.current_dir(&self.cwd);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
}

/// One Bash tool call as the hooks saw it.
pub struct Bash {
    pub pre: Out,
    pub run: Option<Run>,
    pub post: Option<Out>,
}

impl Bash {
    pub fn denied(&self) -> bool {
        self.pre.denied()
    }
    pub fn reason(&self) -> String {
        self.pre.reason()
    }
    /// What PostToolUse told the session.
    pub fn context(&self) -> String {
        self.post.as_ref().map(Out::context).unwrap_or_default()
    }
    pub fn code(&self) -> i32 {
        self.run.as_ref().map(|r| r.code).unwrap_or(-1)
    }
    pub fn output(&self) -> String {
        self.run.as_ref().map(|r| format!("{}{}", r.out, r.err)).unwrap_or_default()
    }
}

/// `PATH` with the directory of the `tcas` under test first, so git hooks
/// and scripts find it.
pub fn path_with_tcas() -> String {
    let dir = Path::new(TCAS).parent().unwrap().display().to_string();
    format!("{dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// One repository at the workspace root, opened.
pub fn one_repo(t: &T) -> PathBuf {
    let r = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("src/auth/x.ts", "x\n"), ("src/auth/y.ts", "y\n"), ("src/util/z.ts", "z\n"), (".gitignore", "dist/\n")]);
    let out = t.cmd(None, &["workspace", "open"]).current_dir(&r).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    r
}

pub fn leases(t: &T) -> Vec<(String, String, String, String)> {
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

pub fn events(t: &T) -> Vec<Value> {
    t.ok(None, &["--json", "log", "-n", "500", "--all"]).json().as_array().unwrap().clone()
}

pub fn commit(dir: &Path, paths: &[&str]) {
    let mut args = vec!["commit", "-qm", "x", "--"];
    args.extend(paths);
    git(dir, &args);
}

