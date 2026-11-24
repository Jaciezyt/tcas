//! End-to-end checks for Milestone 6 (plan.md section 3, harness.md section 10):
//! an AGY session (`tcas hook agy` + `ANTIGRAVITY_CONVERSATION_ID`) and a
//! Claude Code session (`tcas hook claude` + `TCAS_SESSION`) sharing the same
//! checkout over acceptance cases 1–15, plus AGY-specific hooks (`PostInvocation`
//! deferred delivery, `WaitMsBeforeAsync: 500` rewrite, `Stop(fullyIdle)`
//! background window promotion, `invoke_subagent` parent lease sharing,
//! `~/.gemini` built-in exemption, and `plugins/agy/bin/tcas` fail-closed).

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use common::*;

/// An AGY session as its hooks (`plugins/agy/hooks.json`) see it.
/// Note that in AGY, hook processes run with `current_dir` set to the plugin
/// root rather than the repository, while `cwd` in the JSON payload carries
/// the agent's working directory (harness.md section 10).
struct AgyS<'a> {
    t: &'a T,
    id: String,
    cwd: PathBuf,
    plugin_cwd: PathBuf,
    proc: Child,
    calls: std::cell::Cell<u32>,
    last_user_input: std::cell::RefCell<String>,
}

impl Drop for AgyS<'_> {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

struct AgyOut {
    code: i32,
    raw: String,
    v: Value,
    err: String,
}

impl AgyOut {
    fn denied(&self) -> bool {
        self.v["decision"] == "deny"
    }
    fn allowed(&self) -> bool {
        self.v["decision"] == "allow"
    }
    fn passthrough(&self) -> bool {
        self.raw.is_empty()
    }
    fn reason(&self) -> String {
        self.v["reason"].as_str().unwrap_or("").to_string()
    }
    fn context(&self) -> String {
        self.v["injectSteps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|st| st["systemMessage"]["systemMessage"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn is_empty_obj(&self) -> bool {
        self.v.as_object().is_some_and(|o| o.is_empty())
    }
}

struct AgyCmd {
    pre: AgyOut,
    run: Option<Run>,
    post_inv: Option<AgyOut>,
}

impl AgyCmd {
    fn denied(&self) -> bool {
        self.pre.denied()
    }
    fn reason(&self) -> String {
        self.pre.reason()
    }
    fn context(&self) -> String {
        self.post_inv.as_ref().map(AgyOut::context).unwrap_or_default()
    }
}

impl<'a> AgyS<'a> {
    fn at(t: &'a T, id: &str, cwd: &Path) -> AgyS<'a> {
        let proc = Command::new("sleep").arg("600").stdin(Stdio::null()).spawn().unwrap();
        let plugin_cwd = t.root.join("agy-plugin-cwd");
        std::fs::create_dir_all(&plugin_cwd).unwrap();
        AgyS {
            t,
            id: id.into(),
            cwd: cwd.to_path_buf(),
            plugin_cwd,
            proc,
            calls: 0.into(),
            last_user_input: std::cell::RefCell::new("User task".into()),
        }
    }

    fn subagent_of(parent: &AgyS<'a>, sub_id: &str) -> AgyS<'a> {
        // Shares parent's PID via TCAS_HOOK_PID (we spawn a dummy sleep only for Drop,
        // and pass parent.proc.id() in hook_with_pid).
        let proc = Command::new("sleep").arg("600").stdin(Stdio::null()).spawn().unwrap();
        AgyS {
            t: parent.t,
            id: sub_id.into(),
            cwd: parent.cwd.clone(),
            plugin_cwd: parent.plugin_cwd.clone(),
            proc,
            calls: 0.into(),
            last_user_input: std::cell::RefCell::new(String::new()),
        }
    }

    fn call_id(&self) -> String {
        self.calls.set(self.calls.get() + 1);
        format!("call_{}_{}", self.id, self.calls.get())
    }

    fn hook(&self, input: Value) -> AgyOut {
        self.hook_env(input, &[])
    }

    fn hook_env(&self, mut input: Value, env: &[(&str, &str)]) -> AgyOut {
        if input.get("conversationId").is_none() {
            input["conversationId"] = json!(self.id);
        }
        if input.get("cwd").is_none() {
            input["cwd"] = json!(self.cwd);
        }
        if input.get("workspacePaths").is_none() {
            input["workspacePaths"] = json!([self.cwd]);
        }
        if input.get("lastUserInput").is_none() {
            input["lastUserInput"] = json!(*self.last_user_input.borrow());
        }
        input["transcriptPath"] = json!("/dev/null");
        let mut c = Command::new(TCAS);
        // Deliberately run in `plugin_cwd` (NOT `self.cwd`) to verify `hook/agy.rs`
        // reads `cwd` from stdin JSON rather than process pwd!
        c.args(["hook", "agy"])
            .current_dir(&self.plugin_cwd)
            .env("TCAS_HOME", &self.t.home)
            .env("TCAS_HOOK_PID", self.proc.id().to_string())
            .env_remove("TCAS_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .env_remove("ANTIGRAVITY_AGENT")
            .env_remove("TCAS_DISABLE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let v = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("AGY hook output is not JSON ({e}): {raw}"))
        };
        let r = AgyOut { code: o.status.code().unwrap_or(-1), raw, v, err: String::from_utf8_lossy(&o.stderr).into_owned() };
        assert_eq!(r.code, 0, "tcas hook agy exited {}: {}", r.code, r.err);
        r
    }

    fn start(&self, user_input: &str) -> AgyOut {
        *self.last_user_input.borrow_mut() = user_input.into();
        self.hook(json!({ "hookEventName": "SessionStart", "lastUserInput": user_input }))
    }

    fn pre_invocation(&self, user_input: &str) -> AgyOut {
        *self.last_user_input.borrow_mut() = user_input.into();
        self.hook(json!({ "hookEventName": "PreInvocation", "invocationNum": 0, "lastUserInput": user_input }))
    }

    fn post_invocation(&self) -> AgyOut {
        self.hook(json!({ "hookEventName": "PostInvocation", "invocationNum": 0 }))
    }

    fn pre_edit(&self, path: &Path) -> AgyOut {
        let id = self.call_id();
        self.hook(json!({
            "hookEventName": "PreToolUse",
            "toolCall": {
                "id": id,
                "name": "edit_file",
                "args": { "Path": path, "Target": "a", "Replacement": "b" }
            }
        }))
    }

    /// PreToolUse (`write_file`), file write, PostToolUse (`error: ""`), and PostInvocation.
    fn write(&self, path: &Path, text: &str) -> AgyOut {
        let id = self.call_id();
        let tc = json!({ "id": id, "name": "write_file", "args": { "Path": path, "Content": text } });
        let pre = self.hook(json!({ "hookEventName": "PreToolUse", "toolCall": tc }));
        if pre.denied() {
            return pre;
        }
        assert!(pre.passthrough(), "allowed edit must pass through with 0-byte stdout, got: {}", pre.raw);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, text).unwrap();
        let post = self.hook(json!({ "hookEventName": "PostToolUse", "toolCall": tc, "error": "" }));
        assert!(post.is_empty_obj(), "PostToolUse must return {{}}: {}", post.raw);
        self.post_invocation()
    }

    fn read(&self, path: &Path) -> AgyOut {
        let id = self.call_id();
        let tc = json!({ "id": id, "name": "read_file", "args": { "Path": path } });
        let post = self.hook(json!({ "hookEventName": "PostToolUse", "toolCall": tc, "error": "" }));
        assert!(post.is_empty_obj());
        self.post_invocation()
    }

    fn pre_cmd(&self, cmd: &str, wait_ms: Option<i64>) -> AgyOut {
        let id = self.call_id();
        let mut args = json!({ "CommandLine": cmd, "toolSummary": "test" });
        if let Some(w) = wait_ms {
            args["WaitMsBeforeAsync"] = json!(w);
        }
        self.hook(json!({
            "hookEventName": "PreToolUse",
            "toolCall": { "id": id, "name": "run_command", "args": args }
        }))
    }

    fn sh_cmd(&self, command: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", command])
            .current_dir(&self.cwd)
            .env("TCAS_HOME", &self.t.home)
            .env("ANTIGRAVITY_CONVERSATION_ID", &self.id)
            .env("ANTIGRAVITY_AGENT", "1")
            .env("PATH", path_with_tcas())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env_remove("TCAS_SESSION")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("TCAS_ALLOW")
            .stdin(Stdio::null());
        c
    }

    fn cmd_call(&self, command: &str) -> AgyCmd {
        let id = self.call_id();
        let tc = json!({
            "id": id,
            "name": "run_command",
            "args": { "CommandLine": command, "WaitMsBeforeAsync": 5000, "toolSummary": "test" }
        });
        let pre = self.hook(json!({ "hookEventName": "PreToolUse", "toolCall": tc }));
        if pre.denied() {
            return AgyCmd { pre, run: None, post_inv: None };
        }
        let o = self.sh_cmd(command).output().unwrap();
        let run = Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        };
        let post = self.hook(json!({ "hookEventName": "PostToolUse", "toolCall": tc, "error": "" }));
        assert!(post.is_empty_obj());
        let post_inv = self.post_invocation();
        AgyCmd { pre, run: Some(run), post_inv: Some(post_inv) }
    }

    fn stop(&self, fully_idle: bool) -> AgyOut {
        let r = self.hook(json!({
            "hookEventName": "Stop",
            "terminationReason": "NO_TOOL_CALL",
            "fullyIdle": fully_idle,
            "continuationActive": false,
            "error": ""
        }));
        assert!(r.is_empty_obj());
        r
    }

    /// Runs `tcas` as an AGY `run_command` child process (using `ANTIGRAVITY_CONVERSATION_ID`,
    /// NOT `TCAS_SESSION`).
    fn tcas(&self, args: &[&str]) -> Run {
        let mut c = self.t.cmd(None, args);
        c.current_dir(&self.cwd)
            .env("ANTIGRAVITY_CONVERSATION_ID", &self.id)
            .env("ANTIGRAVITY_AGENT", "1");
        let o = c.stdin(Stdio::null()).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn spawn_tcas(&self, args: &[&str]) -> Child {
        let mut c = self.t.cmd(None, args);
        c.current_dir(&self.cwd)
            .env("ANTIGRAVITY_CONVERSATION_ID", &self.id)
            .env("ANTIGRAVITY_AGENT", "1");
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Cases 1–8: Edit interception, directory leases, atomic acquire, orphans,
// resume via PreInvocation, discard, and tcasd unavailable between Claude + AGY.

#[test]
fn claude_and_agy_edit_coordination_cases_1_to_4_and_16() {
    let t = T::new();
    let app = one_repo(&t);
    let agy = AgyS::at(&t, "aaaa1111-2222", &app);
    let claude = S::at(&t, "bbbb3333-4444", &app);

    let intro = agy.start("Implement auth token refresh").context();
    assert!(intro.contains("managed by TCAS"), "{intro}");
    agy.pre_invocation("Implement auth token refresh");
    claude.start("startup");
    claude.prompt("Secret Claude title");

    // Case 1: AGY edits `a.txt` -> holds `a.txt` (`edit`).
    assert!(agy.write(&app.join("a.txt"), "agy A\n").is_empty_obj());
    assert_eq!(leases(&t), [("agy:aaaa1111-2222".into(), "a.txt".into(), "edit".into(), "active".into())]);

    // Case 2 (Claude blocked by AGY, then AGY blocked by Claude):
    let d1 = claude.edit(&app.join("a.txt"), "claude overwrite\n");
    assert!(d1.denied());
    assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "agy A\n");
    assert!(d1.reason().contains("agy:aaaa…"), "{}", d1.reason());
    assert!(!d1.reason().contains("Implement auth token refresh"), "title must stay private: {}", d1.reason());

    assert!(claude.edit(&app.join("b.txt"), "claude B\n").silent());
    claude.tcas(&["lease", "acquire", "b.txt", "--brief", "Update b.txt formatting"]);
    let _ = agy.post_invocation(); // drain broadcast notice

    let d2 = agy.write(&app.join("b.txt"), "agy overwrite\n");
    assert!(d2.denied());
    assert_eq!(std::fs::read_to_string(app.join("b.txt")).unwrap(), "claude B\n");
    let why = d2.reason();
    assert!(why.contains("leased by another session (claude:bbbb…"), "{why}");
    assert!(why.contains("Their brief: \"Update b.txt formatting\""), "{why}");
    assert!(!why.contains("Secret Claude title"), "{why}");
    assert!(
        why.contains("run this as a background command (run_command tool, WaitMsBeforeAsync: 500):\n  tcas lease wait b.txt"),
        "{why}"
    );

    // Case 16 (AGY read_file hint delivered via PostToolUse -> PostInvocation):
    let rh = agy.read(&app.join("b.txt")).context();
    assert!(rh.contains("b.txt is being changed by another session (claude:bbbb…)"), "{rh}");
    assert!(rh.contains("Update b.txt formatting"), "{rh}");
    assert!(agy.read(&app.join("b.txt")).is_empty_obj(), "read hint emitted only once");

    // Case 3: Directory lease held by AGY blocks Claude, and vice-versa.
    assert_eq!(agy.tcas(&["lease", "acquire", "src/auth/", "--brief", "Refactor auth"]).code, 0);
    assert!(claude.pre_edit(&app.join("src/auth/x.ts")).denied());

    // Case 4: AGY requests `src/util/z.ts` + `b.txt` (held by Claude) -> exit 3, atomic none granted.
    let acq = agy.tcas(&["lease", "acquire", "src/util/z.ts", "b.txt"]);
    assert_eq!(acq.code, 3, "{}", acq.out);
    assert!(!leases(&t).iter().any(|l| l.1 == "src/util/z.ts"));
}

#[test]
fn agy_orphan_on_process_exit_resume_via_pre_invocation_and_discard_cases_5_to_8() {
    let t = T::new();
    std::fs::write(
        t.home.join("config.toml"),
        "[daemon]\nliveness_interval_secs = 1\n[exempt]\nbuiltin = false\n",
    )
    .unwrap();
    let app = one_repo(&t);
    let mut agy = AgyS::at(&t, "aaaa1111-2222", &app);
    let claude = S::at(&t, "bbbb3333-4444", &app);
    agy.start("Edit a.txt");
    claude.start("startup");
    agy.write(&app.join("a.txt"), "dirty from AGY\n");

    // Case 5: AGY has no SessionEnd hook; when its process exits, liveness marks it
    // ended and orphans dirty `a.txt`.
    let _ = agy.proc.kill();
    let _ = agy.proc.wait();
    t.until("liveness ends AGY session", || {
        leases(&t).iter().any(|l| l.0 == "agy:aaaa1111-2222" && l.1 == "a.txt" && l.3 == "orphaned")
    });
    let d = claude.pre_edit(&app.join("a.txt"));
    assert!(d.denied());
    assert!(d.reason().contains("held by a session that has ended (agy:aaaa…"), "{}", d.reason());

    // Case 6: `agy -r <cid>` resumes WITHOUT firing SessionStart — its first hook
    // is PreInvocation with the same conversationId and a new PID!
    let mut agy_resumed = AgyS::at(&t, "aaaa1111-2222", &app);
    let ctx = agy_resumed.pre_invocation("Continue editing a.txt").context();
    assert!(ctx.contains("Your leases here:\n  a.txt"), "resume must re-inject held leases: {ctx}");
    assert_eq!(leases(&t)[0].3, "active");
    assert!(!agy_resumed.write(&app.join("a.txt"), "dirty from AGY v2\n").denied());

    // Case 7: AGY exits again; user `git checkout -- a.txt`; Claude edits `a.txt` cleanly.
    let _ = agy_resumed.proc.kill();
    let _ = agy_resumed.proc.wait();
    t.until("liveness orphans again", || leases(&t)[0].3 == "orphaned");
    git(&app, &["checkout", "--", "a.txt"]);
    assert!(!claude.edit(&app.join("a.txt"), "claude takes over\n").denied());
    assert_eq!(leases(&t), [("claude:bbbb3333-4444".into(), "a.txt".into(), "edit".into(), "active".into())]);

    // Case 8: tcasd stopped and cannot start -> AGY workspace edits denied, outside edits pass through.
    let agy3 = AgyS::at(&t, "cccc5555-6666", &app);
    agy3.start("offline check");
    t.ok(None, &["daemon", "stop"]);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&t.home, std::fs::Permissions::from_mode(0o500)).unwrap();
    let blocked = agy3.pre_edit(&app.join("a.txt"));
    let outside = agy3.write(&t.root.join("outside.txt"), "ok\n");
    std::fs::set_permissions(&t.home, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(blocked.denied());
    assert!(blocked.reason().contains("tcasd is not available"), "{}", blocked.reason());
    assert!(!outside.denied());
    assert_eq!(std::fs::read_to_string(t.root.join("outside.txt")).unwrap(), "ok\n");
}

// ---------------------------------------------------------------------------
// Cases 9–15: `run_command` reconcile, overreach, git guard, `tcas commit`,
// background `Stop(fullyIdle=false)` -> `PostToolUse` at exit, branch switch.

#[test]
fn claude_and_agy_bash_reconcile_guard_commit_bg_and_branch_cases_9_to_15() {
    let t = T::new();
    let app = one_repo(&t);
    let agy = AgyS::at(&t, "aaaa1111-2222", &app);
    let claude = S::at(&t, "bbbb3333-4444", &app);
    agy.start("Work in shared repo");
    claude.start("startup");

    // Case 12: Only AGY has leases (or nobody does) -> `git add -A` is allowed.
    let alone_add = agy.cmd_call("git add -A");
    assert!(!alone_add.denied());

    // Case 9: AGY writes `c.txt` via `run_command` -> reconciled to `agy:aaaa1111-2222` (`bash`).
    let c9 = agy.cmd_call("printf 'from agy cmd\\n' > c.txt");
    assert!(!c9.denied());
    assert!(leases(&t).iter().any(|l| l.0 == "agy:aaaa1111-2222" && l.1 == "c.txt" && l.2 == "bash"));

    // Claude takes `b.txt` and stages it (`git add b.txt`).
    claude.edit(&app.join("b.txt"), "claude b\n");
    assert!(!claude.bash("git add b.txt").denied());

    // Case 11: Now Claude also holds a lease in `app`, so AGY `git add -A` is blocked by guard!
    let c11 = agy.cmd_call("git add -A");
    assert!(c11.denied());
    assert!(c11.reason().contains("tcas commit"), "{}", c11.reason());

    // Case 10: AGY overwrites Claude's `b.txt` via `run_command` -> overreach!
    // Hint for AGY is deferred on PostToolUse and injected on PostInvocation; Claude gets notice.
    let c10 = agy.cmd_call("printf 'overreach\\n' >> b.txt");
    assert!(!c10.denied());
    assert!(c10.context().contains("your last command changed files that other sessions hold"), "{}", c10.context());
    let claude_notice = claude.post_other().context();
    assert!(claude_notice.contains("changed files you hold with a Bash command"), "{claude_notice}");
    assert!(claude_notice.contains("printf 'overreach\\n' >> b.txt"), "{claude_notice}");
    assert!(events(&t).iter().any(|e| e["type"] == "bash.overreach"));
    // Restore `b.txt` to Claude's staged version so Case 13 checks clean staging isolation.
    std::fs::write(app.join("b.txt"), "claude b\n").unwrap();

    // Case 13: AGY also edits `a.txt` and runs `tcas commit -m "agy commit"`:
    // commits ONLY `a.txt` and `c.txt`, leaves `b.txt` staged for Claude, releases AGY's file leases!
    agy.write(&app.join("a.txt"), "agy a\n");
    let commit = agy.cmd_call("tcas commit -m 'agy commit'");
    assert!(!commit.denied());
    assert_eq!(commit.run.as_ref().unwrap().code, 0, "{:?}", commit.run.as_ref().map(|r| (&r.out, &r.err)));
    let committed_files = git_text(&app, &["show", "--name-only", "--pretty=format:", "HEAD"]);
    assert_eq!(committed_files.lines().collect::<Vec<_>>(), vec!["a.txt", "c.txt"]);
    let cached = git_text(&app, &["diff", "--cached", "--name-only"]);
    assert_eq!(cached.trim(), "b.txt", "Claude's staged b.txt must remain in the index");
    assert!(!leases(&t).iter().any(|l| l.0 == "agy:aaaa1111-2222"));

    // Case 14: AGY starts a background `run_command` (`WaitMsBeforeAsync: 500`),
    // hits `Stop(fullyIdle: false)` BEFORE `bg.txt` is written, then writes `bg.txt`
    // and fires `PostToolUse` on exit -> `agy` gets `bg.txt` (`bash`)!
    let bg_id = agy.call_id();
    let bg_cmd = "sleep 0.6 && printf 'bg\\n' > bg.txt";
    let bg_tc = json!({
        "id": bg_id,
        "name": "run_command",
        "args": { "CommandLine": bg_cmd, "WaitMsBeforeAsync": 500, "toolSummary": "bg write" }
    });
    assert!(agy.hook(json!({ "hookEventName": "PreToolUse", "toolCall": bg_tc })).passthrough());
    let mut bg_child = agy.sh_cmd(bg_cmd).spawn().unwrap();
    // Turn ends while command is still sleeping (`fullyIdle: false`)!
    agy.stop(false);
    assert!(!app.join("bg.txt").exists());
    bg_child.wait().unwrap();
    assert!(app.join("bg.txt").is_file());
    // Command finished -> AGY fires PostToolUse followed by wake-up PreInvocation & Stop(fullyIdle: true)
    assert!(agy.hook(json!({ "hookEventName": "PostToolUse", "toolCall": bg_tc, "error": "" })).is_empty_obj());
    agy.pre_invocation("Work in shared repo");
    agy.stop(true);
    assert!(
        leases(&t).iter().any(|l| l.0 == "agy:aaaa1111-2222" && l.1 == "bg.txt" && l.2 == "bash"),
        "expected bg.txt bash lease: {:?}",
        leases(&t)
    );

    // Case 15: Branch switch reported (`branch.changed` event + notice delivered via PreInvocation/PostInvocation).
    // User switches branch in `app`; both AGY and Claude hold leases there!
    git(&app, &["checkout", "-q", "-b", "feature/m6"]);
    // Trigger discovery on Claude Bash call -> AGY gets `branch.changed` notice on next PreInvocation!
    claude.bash("true");
    assert!(events(&t).iter().any(|e| e["type"] == "branch.changed"));
    let br_ctx = agy.pre_invocation("Next turn").context();
    assert!(br_ctx.contains("branch of . changed") && br_ctx.contains("feature/m6"), "{br_ctx}");
}

// ---------------------------------------------------------------------------
// AGY-specific mechanics: `WaitMsBeforeAsync: 500` rewrite, background wait
// wakeup via PreInvocation, subagent lease sharing (`invoke_subagent`),
// `~/.gemini` built-in exempt, ancestor identity, and forwarder fail-closed.

#[test]
fn agy_wait_rewrite_wakeup_subagent_and_gemini_exempt() {
    let t = T::new();
    let app = one_repo(&t);
    let agy = AgyS::at(&t, "aaaa1111-2222", &app);
    let claude = S::at(&t, "bbbb3333-4444", &app);
    agy.start("Edit a.txt");
    claude.start("startup");

    // 1. `tcas` command auto-approval & `WaitMsBeforeAsync: 500` rewrite (Case 28)
    let rw = agy.pre_cmd("tcas lease wait a.txt --brief 'need a'", Some(5000));
    assert!(rw.allowed(), "{}", rw.raw);
    assert_eq!(rw.v["overwrite"]["WaitMsBeforeAsync"], 500, "{}", rw.raw);

    let rw_unset = agy.pre_cmd("tcas lease wait a.txt", None);
    assert!(rw_unset.allowed());
    assert_eq!(rw_unset.v["overwrite"]["WaitMsBeforeAsync"], 500);

    let already_bg = agy.pre_cmd("tcas lease wait a.txt", Some(500));
    assert!(already_bg.allowed());
    assert!(already_bg.v.get("overwrite").is_none(), "{}", already_bg.raw);

    assert!(agy.pre_cmd("tcas status", Some(5000)).allowed());
    assert!(agy.pre_cmd("tcas commit -m x", Some(5000)).passthrough(), "commit must not be auto-approved");
    let compound = agy.pre_cmd("tcas lease wait a.txt && echo ok", Some(5000));
    assert!(compound.denied());
    assert!(compound.reason().contains("WaitMsBeforeAsync: 500"), "{}", compound.reason());

    // 2. Background wait granted on Claude commit -> notice delivered on AGY wakeup PreInvocation!
    claude.edit(&app.join("a.txt"), "claude v1\n");
    let waiter = agy.spawn_tcas(&["lease", "wait", "a.txt", "--timeout", "30", "--brief", "append footer"]);
    t.until("AGY wait registered", || t.daemon_waits() == 1);
    agy.stop(false);
    assert_eq!(claude.tcas(&["commit", "-m", "claude v1"]).code, 0);
    assert_eq!(finish(waiter).code, 0);
    let wake_ctx = agy.pre_invocation("Edit a.txt").context();
    assert!(wake_ctx.contains("your wait is over") && wake_ctx.contains("a.txt"), "{wake_ctx}");

    // 3. AGY `invoke_subagent`: child conversation in same `agy` process (`lastUserInput: ""`)
    // shares parent session's leases and CLI identity!
    assert_eq!(agy.tcas(&["lease", "acquire", "src/auth/", "--brief", "auth work"]).code, 0);
    let sub = AgyS::subagent_of(&agy, "sub9999-0000");
    let pid_s = agy.proc.id().to_string();
    let sub_start = sub.hook_env(json!({ "hookEventName": "SessionStart", "lastUserInput": "" }), &[("TCAS_HOOK_PID", &pid_s)]);
    assert!(!sub_start.denied());
    let sub_pre = sub.hook_env(
        json!({
            "hookEventName": "PreToolUse",
            "lastUserInput": "",
            "toolCall": { "id": "sub_1", "name": "write_file", "args": { "Path": app.join("src/auth/x.ts"), "Content": "sub edit\n" } }
        }),
        &[("TCAS_HOOK_PID", &pid_s)],
    );
    assert!(sub_pre.passthrough(), "subagent must share parent's src/auth/ lease, got: {}", sub_pre.raw);
    std::fs::write(app.join("src/auth/x.ts"), "sub edit\n").unwrap();
    sub.hook_env(
        json!({
            "hookEventName": "PostToolUse",
            "lastUserInput": "",
            "error": "",
            "toolCall": { "id": "sub_1", "name": "write_file", "args": { "Path": app.join("src/auth/x.ts"), "Content": "sub edit\n" } }
        }),
        &[("TCAS_HOOK_PID", &pid_s)],
    );
    // Lease is recorded under parent `agy:aaaa1111-2222`, NOT `agy:sub9999-0000`!
    assert!(leases(&t).iter().any(|l| l.0 == "agy:aaaa1111-2222" && l.1 == "src/auth/x.ts"));
    assert!(!leases(&t).iter().any(|l| l.0 == "agy:sub9999-0000"));
    // Subagent running `tcas whoami` with `ANTIGRAVITY_CONVERSATION_ID=sub9999-0000` resolves to parent!
    let who = sub.tcas(&["--json", "whoami"]).json();
    assert_eq!(who["caller"], "agy:aaaa1111-2222");
    // Subagent `Stop` does NOT mark the parent session idle while parent is active!
    sub.hook_env(
        json!({ "hookEventName": "Stop", "lastUserInput": "", "terminationReason": "NO_TOOL_CALL", "fullyIdle": true }),
        &[("TCAS_HOOK_PID", &pid_s)],
    );
    let sess_list = t.ok(None, &["--json", "session", "list"]).json();
    let parent_row = sess_list.as_array().unwrap().iter().find(|s| s["id"] == "agy:aaaa1111-2222").unwrap();
    assert_eq!(parent_row["state"], "active", "subagent Stop must not make parent idle: {parent_row}");

    // 4. Forwarder `plugins/agy/bin/tcas` fail-closed when `tcas` binary is missing:
    let lonely = t.root.join("lonely-agy");
    std::fs::create_dir_all(lonely.join("bin")).unwrap();
    let agy_fwd = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/agy/bin/tcas");
    std::fs::copy(agy_fwd, lonely.join("bin/tcas")).unwrap();
    let run_fwd = |home: &Path, ev_json: &str| {
        let mut c = Command::new("sh");
        c.arg(lonely.join("bin/tcas"))
            .args(["hook", "agy"])
            .env("TCAS_HOME", home)
            .env_remove("TCAS_BIN")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        ch.stdin.take().unwrap().write_all(ev_json.as_bytes()).unwrap();
        String::from_utf8_lossy(&ch.wait_with_output().unwrap().stdout).trim().to_string()
    };
    let empty_home = t.root.join("empty-agy-home");
    std::fs::create_dir_all(&empty_home).unwrap();
    assert_eq!(run_fwd(&empty_home, r#"{"hookEventName":"PreToolUse"}"#), "");
    assert_eq!(run_fwd(&empty_home, r#"{"hookEventName":"PostToolUse"}"#), "{}");
    std::fs::write(empty_home.join("workspaces.json"), r#"{"version":1,"workspaces":[{"id":"x"}],"exempt":[]}"#).unwrap();
    let denied_out: Value = serde_json::from_str(&run_fwd(&empty_home, r#"{"hookEventName":"PreToolUse"}"#)).unwrap();
    assert_eq!(denied_out["decision"], "deny");
    assert_eq!(run_fwd(&empty_home, r#"{"hookEventName":"Stop"}"#), "{}");
}

#[test]
fn builtin_gemini_exempt_and_tcas_disable() {
    let t = T::new();
    let uhome = t.root.join("uhome");
    std::fs::create_dir_all(uhome.join(".gemini/antigravity-cli/brain/conv1")).unwrap();
    // Enable built-in exemptions in this home:
    std::fs::write(t.home.join("config.toml"), "[exempt]\nbuiltin = true\n").unwrap();
    let app = t.repo("app", &[("a.txt", "a\n")]);
    // Open workspace (note: `app` is under `/tmp`, which is built-in exempt! So test `exempt list` directly):
    let ex = t.cmd(None, &["--json", "exempt", "list"]).env("HOME", &uhome).output().unwrap();
    assert!(ex.status.success(), "{}", String::from_utf8_lossy(&ex.stderr));
    let ej: Value = serde_json::from_slice(&ex.stdout).unwrap();
    let entries = ej["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|e| e["written"] == "~/.gemini" && e["source"] == "built-in: harness directory"),
        "expected ~/.gemini in built-in exempt list: {ej}"
    );

    // TCAS_DISABLE returns 0-byte stdout on PreToolUse and `{}` on other AGY hooks:
    let agy = AgyS::at(&t, "dddd7777-8888", &app);
    let pre = agy.hook_env(
        json!({ "hookEventName": "PreToolUse", "toolCall": { "id": "1", "name": "write_file", "args": { "Path": app.join(".tcas.toml") } } }),
        &[("TCAS_DISABLE", "1")],
    );
    assert!(pre.passthrough());
    let post = agy.hook_env(json!({ "hookEventName": "PostToolUse" }), &[("TCAS_DISABLE", "1")]);
    assert!(post.is_empty_obj());
}
