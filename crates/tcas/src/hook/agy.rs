//! AGY's (Antigravity CLI / IDE) side of `tcas hook` (harness.md section 10;
//! input and output formats as measured in tests/probe/RESULTS.md).

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tcas_core::guard;
use tcas_core::proto::{HookRequest, HookResponse, HookTool};
use tcas_core::shell;

use super::{Local, abs, contact, harness_pid, unavailable};

const EDIT_TOOLS: &[&str] = &[
    "edit_file",
    "write_file",
    "write_to_file",
    "replace_file_content",
    "multi_replace_file_content",
];

const READ_TOOLS: &[&str] = &[
    "read_file",
    "view_file",
    "view_file_outline",
    "view_code_item",
    "grep_search",
    "find_by_name",
    "list_dir",
];

/// The subcommands a session's `tcas` command is approved for (harness.md
/// section 5.5).
const APPROVED: &[&[&str]] = &[
    &["lease", "acquire"],
    &["lease", "release"],
    &["lease", "list"],
    &["lease", "who"],
    &["lease", "wait"],
    &["status"],
    &["workspace", "list"],
    &["session", "list"],
    &["exempt", "list"],
    &["log"],
    &["stats"],
    &["doctor"],
    &["whoami"],
];

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

/// The timeout configured for the hook in hooks.json, in seconds.
fn configured_timeout(event: &str, tool: &str) -> u64 {
    match (event, tool) {
        ("SessionStart" | "PreInvocation" | "PostInvocation" | "Stop", _) => 5,
        ("PreToolUse", "run_command") => 10,
        ("PreToolUse", _) => 8,
        ("PostToolUse", "run_command") => 10,
        ("PostToolUse", _) => 5,
        _ => 5,
    }
}

fn print(v: Value) {
    println!("{v}");
}

fn empty_json() {
    print(json!({}));
}

fn inject_context(text: Option<String>) {
    match text.filter(|t| !t.is_empty()) {
        Some(t) => print(json!({
            "injectSteps": [
                {
                    "systemMessage": {
                        "systemMessage": t
                    }
                }
            ]
        })),
        None => empty_json(),
    }
}

fn decision(dec: &str, reason: &str, overwrite: Option<Value>) {
    let mut o = json!({
        "decision": dec,
        "reason": reason,
    });
    if let Some(ov) = overwrite {
        o["overwrite"] = ov;
    }
    print(o);
}

/// `TCAS_DISABLE` is set: `PreToolUse` prints nothing (pass-through); all
/// other lifecycle hooks output `{}`.
pub fn disabled(input: &str) -> i32 {
    let is_pre_tool = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.get("hookEventName").and_then(Value::as_str).map(|s| s == "PreToolUse"))
        .unwrap_or_else(|| input.contains("\"PreToolUse\""));
    if !is_pre_tool {
        empty_json();
    }
    0
}

/// The denial as the model reads it: tcasd's reason, then how to wait with
/// AGY's background commands (`run_command`, `WaitMsBeforeAsync: 500`).
fn denial_text(r: &HookResponse) -> String {
    let mut t = r.reason.clone().unwrap_or_else(|| "tcas: refused.".into());
    if let Some(w) = &r.wait_command {
        let (it, file) = if r.wait_many { ("them", "these files") } else { ("it", "this file") };
        if r.needs_user {
            t.push_str(&format!(
                "\nTo be told when {it} {} free, run this as a background command (run_command tool, \
                 WaitMsBeforeAsync: 500):\n  {w}\nIt returns only after the user has dealt with {it}.",
                if r.wait_many { "are" } else { "is" }
            ));
        } else {
            t.push_str(&format!(
                "\nTo wait for {it}, run this as a background command (run_command tool, WaitMsBeforeAsync: 500):\n  \
                 {w}\nThen keep working on something that does not depend on {file}, or end your turn. You will\nbe \
                 told when the lease is yours. Otherwise tell the user."
            ));
        }
    }
    t
}

fn any_agy_marked(local: &Local) -> bool {
    std::fs::read_dir(local.home.sessions_dir())
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("agy:"))
}

pub fn run(input: &str) -> i32 {
    let started = Instant::now();
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => {
            if input.contains("\"PreToolUse\"") {
                decision(
                    "deny",
                    &format!("tcas: could not parse PreToolUse hook input ({e}); refusing tool call to stay safe."),
                    None,
                );
                return 0;
            }
            eprintln!("tcas hook agy: the input is not JSON: {e}");
            empty_json();
            return 0;
        }
    };
    let event = s(&v, "hookEventName").unwrap_or("").to_string();
    let Some(session) = s(&v, "conversationId").map(str::to_string) else {
        if event == "PreToolUse" {
            decision("deny", "tcas: PreToolUse hook input has no conversationId; refusing tool call.", None);
            return 0;
        }
        empty_json();
        return 0;
    };
    let sid = format!("agy:{session}");
    // Important (harness.md section 10): the hook process's own pwd is the
    // plugin root, NOT the agent's working directory; always read `cwd` from JSON.
    let cwd_raw = s(&v, "cwd")
        .map(str::to_string)
        .or_else(|| {
            v.get("workspacePaths")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "/".into());
    let cwd = std::fs::canonicalize(&cwd_raw).map(|p| p.to_string_lossy().into_owned()).unwrap_or(cwd_raw);

    let tc = v.get("toolCall").cloned().unwrap_or(Value::Null);
    let tool_name = s(&tc, "name").unwrap_or("").to_string();
    let call_id = s(&tc, "id").unwrap_or("").to_string();
    let args = tc.get("args").cloned().unwrap_or(Value::Null);
    let last_user_input = s(&v, "lastUserInput").map(str::to_string);
    // In AGY, `invoke_subagent` conversations carry `lastUserInput == ""` on
    // every hook event while sharing the parent's `agy` process.
    let subagent_candidate = last_user_input.is_none();

    let deadline =
        started + Duration::from_secs(configured_timeout(&event, &tool_name)).saturating_sub(Duration::from_secs(1));
    let local = Local::load();
    let (pid, pid_started_at) = harness_pid();
    let base = HookRequest {
        harness: "agy".into(),
        event: String::new(),
        session: session.clone(),
        cwd: cwd.clone(),
        pid,
        pid_started_at,
        subagent_candidate,
        ..HookRequest::default()
    };
    let known = local.marked(&sid) || local.managed(&cwd) || (subagent_candidate && any_agy_marked(&local));

    match event.as_str() {
        "SessionStart" => {
            if !known {
                empty_json();
                return 0;
            }
            let h = HookRequest { event: "session_start".into(), source: Some("startup".into()), ..base };
            match contact(&local, &h, deadline) {
                Ok(r) => inject_context(r.context),
                Err(_) => empty_json(),
            }
            0
        }
        "PreInvocation" => {
            if !known {
                empty_json();
                return 0;
            }
            let h = HookRequest { event: "prompt".into(), prompt: last_user_input, ..base };
            match contact(&local, &h, deadline) {
                Ok(r) => inject_context(r.context),
                Err(_) => empty_json(),
            }
            0
        }
        "PostInvocation" => {
            if !known {
                empty_json();
                return 0;
            }
            let h = HookRequest { event: "post_invocation".into(), ..base };
            match contact(&local, &h, deadline) {
                Ok(r) => inject_context(r.context),
                Err(_) => empty_json(),
            }
            0
        }
        "PreToolUse" if EDIT_TOOLS.contains(&tool_name.as_str()) => {
            let paths = edit_paths(&args, &cwd);
            for p in &paths {
                if let Some(reason) = local.protected(p) {
                    decision("deny", &reason, None);
                    return 0;
                }
            }
            if !paths.iter().any(|p| local.managed(p)) {
                // 0-byte stdout passes through to AGY's own permission flow.
                return 0;
            }
            let tool = HookTool {
                kind: "edit".into(),
                call_id,
                name: Some(tool_name),
                paths,
                ..HookTool::default()
            };
            let h = HookRequest { event: "pre_tool".into(), tool: Some(tool), ..base };
            match contact(&local, &h, deadline) {
                Ok(r) if r.decision.as_deref() == Some("deny") => decision("deny", &denial_text(&r), None),
                Ok(_) => {}
                Err(e) => decision("deny", &unavailable(&e), None),
            }
            0
        }
        "PreToolUse" if tool_name == "run_command" => {
            pre_command(&local, &args, &call_id, &cwd, &sid, base, deadline);
            0
        }
        "PreToolUse" => {
            // Any other tool: 0-byte stdout lets AGY proceed normally.
            0
        }
        "PostToolUse" => {
            let failed = s(&v, "error").is_some();
            let tool = match tool_name.as_str() {
                t if EDIT_TOOLS.contains(&t) => {
                    let paths = edit_paths(&args, &cwd);
                    let outside =
                        local.managed(&cwd) && paths.iter().any(|p| local.root_of(p).is_none() && !local.is_exempt(p));
                    if !(known || paths.iter().any(|p| local.managed(p)) || outside) {
                        empty_json();
                        return 0;
                    }
                    HookTool {
                        kind: "edit".into(),
                        call_id,
                        name: Some(tool_name),
                        paths,
                        failed,
                        ..HookTool::default()
                    }
                }
                "run_command" => {
                    if !known {
                        empty_json();
                        return 0;
                    }
                    // In AGY, `PostToolUse` for `run_command` fires when the command
                    // actually terminates (even if it transitioned to background), so
                    // `background_id` is `None` here: `post_tool` closes and reconciles
                    // the window immediately.
                    HookTool {
                        kind: "bash".into(),
                        call_id,
                        name: Some(tool_name),
                        command: cmd_str(&args).map(str::to_string),
                        background: is_bg_args(&args),
                        background_id: None,
                        failed,
                        ..HookTool::default()
                    }
                }
                t if READ_TOOLS.contains(&t) => {
                    let paths = read_paths(&args, &cwd);
                    if !(known || paths.iter().any(|p| local.managed(p))) {
                        empty_json();
                        return 0;
                    }
                    HookTool {
                        kind: "read".into(),
                        call_id,
                        name: Some(tool_name),
                        paths,
                        failed,
                        ..HookTool::default()
                    }
                }
                _ => {
                    if !known {
                        empty_json();
                        return 0;
                    }
                    HookTool { kind: "other".into(), call_id, name: Some(tool_name), failed, ..HookTool::default() }
                }
            };
            let h = HookRequest { event: "post_tool".into(), tool: Some(tool), defer_context: true, ..base };
            let _ = contact(&local, &h, deadline);
            empty_json();
            0
        }
        "Stop" => {
            if !known {
                empty_json();
                return 0;
            }
            let fully_idle = v.get("fullyIdle").and_then(Value::as_bool).unwrap_or(true);
            let h = if fully_idle {
                HookRequest { event: "stop".into(), background_running: Some(vec![]), ..base }
            } else {
                HookRequest { event: "stop".into(), keep_background: true, ..base }
            };
            let _ = contact(&local, &h, deadline);
            empty_json();
            0
        }
        _ => {
            empty_json();
            0
        }
    }
}

fn edit_paths(args: &Value, cwd: &str) -> Vec<String> {
    ["Path", "TargetFile", "AbsolutePath", "file_path"]
        .iter()
        .filter_map(|k| s(args, k))
        .map(|p| abs(cwd, p))
        .collect()
}

fn read_paths(args: &Value, cwd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in ["Path", "AbsolutePath", "SearchPath", "SearchDirectory", "DirectoryPath", "file_path", "path"] {
        if let Some(p) = s(args, k) {
            let a = abs(cwd, p);
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

fn cmd_str(args: &Value) -> Option<&str> {
    s(args, "CommandLine").or_else(|| s(args, "command"))
}

fn is_bg_args(args: &Value) -> bool {
    let is_daemon = args.get("IsDaemon").and_then(Value::as_bool).unwrap_or(false);
    let wait_ms = args.get("WaitMsBeforeAsync").and_then(Value::as_i64);
    is_daemon || wait_ms.is_some_and(|w| w <= 500)
}

fn pre_command(
    local: &Local,
    args: &Value,
    call_id: &str,
    cwd: &str,
    sid: &str,
    base: HookRequest,
    deadline: Instant,
) {
    let Some(command) = cmd_str(args) else { return };
    let background = is_bg_args(args);
    let home = tcas_sys::home::user_home().map(|h| h.to_string_lossy().into_owned());
    let parsed = shell::parse(command, cwd, home.as_deref());
    let approve = local.ws_config(cwd).is_none_or(|c| c.bash.approve_tcas);

    if let Some(tcas_args) = guard::tcas_command(&parsed) {
        let words: Vec<&str> = tcas_args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
        let is_wait = words.len() >= 2 && words[0] == "lease" && words[1] == "wait";
        let approved = APPROVED.iter().any(|sub| words.len() >= sub.len() && words[..sub.len()] == **sub);
        if is_wait && !background {
            if approve {
                decision(
                    "allow",
                    "tcas lease wait runs in the background (tcas)",
                    Some(json!({ "WaitMsBeforeAsync": 500 })),
                );
            } else {
                decision("deny", &wait_in_background(command), None);
            }
            return;
        }
        if approved && approve {
            decision("allow", "tcas command for sessions (tcas)", None);
        }
        return;
    }
    if guard::has_tcas_wait(&parsed) && !background {
        decision("deny", &wait_in_background(command), None);
        return;
    }
    if let Some(reason) = protected_in(local, &parsed) {
        decision("deny", &reason, None);
        return;
    }

    let mut dirs: Vec<String> = vec![cwd.to_string()];
    for c in &parsed.commands {
        dirs.push(c.dir.clone());
        if let Some(g) = guard::git_call(c) {
            dirs.push(g.dir.clone());
        }
    }
    let concerns = dirs.iter().any(|d| local.managed(d));
    let known = local.marked(sid) || (base.subagent_candidate && any_agy_marked(local));
    if !(concerns || known) {
        return;
    }
    let bg_id = background.then(|| call_id.to_string()).filter(|id| !id.is_empty());
    let tool = HookTool {
        kind: "bash".into(),
        call_id: call_id.to_string(),
        name: Some("run_command".into()),
        command: Some(command.to_string()),
        background,
        background_id: bg_id,
        ..HookTool::default()
    };
    let h = HookRequest { event: "pre_tool".into(), tool: Some(tool), ..base };
    match contact(local, &h, deadline) {
        Ok(r) if r.decision.as_deref() == Some("deny") => decision("deny", &denial_text(&r), None),
        Ok(_) => {}
        Err(e) if concerns => decision("deny", &unavailable(&e), None),
        Err(_) => {}
    }
}

fn protected_in(local: &Local, parsed: &shell::Parsed) -> Option<String> {
    for c in &parsed.commands {
        let args: Vec<&str> =
            if guard::is_read_only(c) { vec![] } else { c.words.iter().skip(1).map(|w| w.text()).collect() };
        for w in args.into_iter().chain(c.redirects.iter().map(|w| w.text())) {
            let w = if w.starts_with('-') { w.split_once('=').map(|(_, v)| v).unwrap_or("") } else { w };
            if w.is_empty() {
                continue;
            }
            if let Some(r) = local.protected(&shell::join_dir(&c.dir, w)) {
                return Some(r);
            }
        }
    }
    None
}

fn wait_in_background(command: &str) -> String {
    let one: String = command.lines().next().unwrap_or("").chars().take(200).collect();
    format!(
        "tcas: tcas lease wait can run for a long time and must not hold up this turn. Run it on its own, as a \
         background command (run_command tool, WaitMsBeforeAsync: 500), not inside a longer command line:\n  {one}"
    )
}
