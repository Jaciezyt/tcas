//! Claude Code's side of `tcas hook` (harness.md sections 3 to 7; input and
//! output formats as measured in tests/probe/RESULTS.md).

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tcas_core::guard;
use tcas_core::proto::{HookRequest, HookResponse, HookTool};
use tcas_core::shell;

use super::{Local, abs, approved, bash_dirs, contact, harness_pid, protected_in, unavailable};

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

/// The timeout configured for the hook in hooks.json, in seconds.
fn configured_timeout(event: &str, tool: &str) -> u64 {
    match (event, tool) {
        ("SessionStart", _) => 5,
        ("UserPromptSubmit", _) => 3,
        ("DirectoryAdded", _) => 5,
        ("PreToolUse", "Bash") => 10,
        ("PreToolUse", _) => 8,
        ("PostToolUse", "Bash") => 10,
        ("PostToolUse", _) => 3,
        ("PostToolUseFailure", _) => 10,
        _ => 5,
    }
}

fn print(v: Value) {
    println!("{v}");
}

fn context(event: &str, text: Option<String>) {
    if let Some(t) = text.filter(|t| !t.is_empty()) {
        print(json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": t } }));
    }
}

fn decision(decision: &str, reason: &str, updated: Option<Value>) {
    let mut o = json!({
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
        "permissionDecisionReason": reason,
    });
    if let Some(u) = updated {
        o["updatedInput"] = u;
    }
    print(json!({ "hookSpecificOutput": o }));
}

/// The denial as the model reads it: tcasd's reason, then how to wait with
/// Claude Code's background commands (harness.md section 4).
fn denial_text(r: &HookResponse) -> String {
    let mut t = r.reason.clone().unwrap_or_else(|| "tcas: refused.".into());
    if let Some(w) = &r.wait_command {
        let (it, file) = if r.wait_many { ("them", "these files") } else { ("it", "this file") };
        if r.needs_user {
            t.push_str(&format!(
                "\nTo be told when {it} {} free, run this as a background command (Bash tool, run_in_background):\n  \
                 {w}\nIt returns only after the user has dealt with {it}.",
                if r.wait_many { "are" } else { "is" }
            ));
        } else {
            t.push_str(&format!(
                "\nTo wait for {it}, run this as a background command (Bash tool, run_in_background):\n  {w}\nThen \
                 keep working on something that does not depend on {file}, or end your turn. You will\nbe told when \
                 the lease is yours. Otherwise tell the user."
            ));
        }
    }
    t
}

pub fn run(input: &str) -> i32 {
    let started = Instant::now();
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tcas hook claude: the input is not JSON: {e}");
            return 1;
        }
    };
    let event = s(&v, "hook_event_name").unwrap_or("").to_string();
    let Some(session) = s(&v, "session_id").map(str::to_string) else {
        eprintln!("tcas hook claude: no session_id in the input");
        return 1;
    };
    let sid = format!("claude:{session}");
    let cwd_raw = s(&v, "cwd").map(str::to_string).unwrap_or_else(|| {
        std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into())
    });
    let cwd = std::fs::canonicalize(&cwd_raw).map(|p| p.to_string_lossy().into_owned()).unwrap_or(cwd_raw);
    let tool_name = s(&v, "tool_name").unwrap_or("").to_string();
    let deadline = started + Duration::from_secs(configured_timeout(&event, &tool_name)).saturating_sub(Duration::from_secs(1));
    let local = Local::load();
    let (pid, pid_started_at) = harness_pid();
    let base = HookRequest {
        harness: "claude".into(),
        event: String::new(),
        session: session.clone(),
        agent: s(&v, "agent_id").map(str::to_string),
        cwd: cwd.clone(),
        pid,
        pid_started_at,
        ..HookRequest::default()
    };
    // tcasd knows this session, or something here concerns a workspace.
    let known = local.marked(&sid) || local.managed(&cwd);

    match event.as_str() {
        "SessionStart" => {
            write_env(&sid);
            let source = s(&v, "source").map(str::to_string);
            let clear = source.as_deref() == Some("clear") && local.has_workspaces();
            if !(known || clear) {
                return 0;
            }
            let workspace_dirs = startup_workspace_dirs(&v, pid, &cwd);
            let h = HookRequest { event: "session_start".into(), source, workspace_dirs, ..base };
            if let Ok(r) = contact(&local, &h, deadline) {
                context("SessionStart", r.context);
            }
            0
        }
        "DirectoryAdded" => {
            if !known {
                return 0;
            }
            let Some(dir) = s(&v, "directory") else {
                return 0;
            };
            let h = HookRequest { event: "dir_added".into(), workspace_dirs: vec![abs(&cwd, dir)], ..base };
            let _ = contact(&local, &h, deadline);
            0
        }
        "UserPromptSubmit" => {
            if !known {
                return 0;
            }
            let h = HookRequest { event: "prompt".into(), prompt: s(&v, "prompt").map(str::to_string), ..base };
            if let Ok(r) = contact(&local, &h, deadline) {
                context("UserPromptSubmit", r.context);
            }
            0
        }
        "PreToolUse" if EDIT_TOOLS.contains(&tool_name.as_str()) => {
            let paths = edit_paths(&v, &cwd);
            for p in &paths {
                if let Some(reason) = local.protected(p) {
                    decision("deny", &reason, None);
                    return 0;
                }
            }
            if !paths.iter().any(|p| local.managed(p)) {
                return 0;
            }
            let tool = HookTool {
                kind: "edit".into(),
                call_id: s(&v, "tool_use_id").unwrap_or("").to_string(),
                name: Some(tool_name.clone()),
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
        "PreToolUse" if tool_name == "Bash" => {
            pre_bash(&local, &v, &cwd, &sid, base, deadline);
            0
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let failed = event == "PostToolUseFailure";
            let call_id = s(&v, "tool_use_id").unwrap_or("").to_string();
            let tool = match tool_name.as_str() {
                t if EDIT_TOOLS.contains(&t) => {
                    let paths = edit_paths(&v, &cwd);
                    let outside = local.managed(&cwd)
                        && paths.iter().any(|p| local.root_of(p).is_none() && !local.is_exempt(p));
                    if !(known || paths.iter().any(|p| local.managed(p)) || outside) {
                        return 0;
                    }
                    HookTool { kind: "edit".into(), call_id, name: Some(tool_name.clone()), paths, failed, ..HookTool::default() }
                }
                "Bash" => {
                    if !known {
                        return 0;
                    }
                    let resp = v.get("tool_response").cloned().unwrap_or(Value::Null);
                    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
                    HookTool {
                        kind: "bash".into(),
                        call_id,
                        name: Some(tool_name.clone()),
                        command: s(&input, "command").map(str::to_string),
                        background: input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false),
                        background_id: s(&resp, "backgroundTaskId").map(str::to_string),
                        failed,
                        ..HookTool::default()
                    }
                }
                "Read" | "Grep" => {
                    let paths = read_paths(&v, &cwd);
                    if !(known || paths.iter().any(|p| local.managed(p))) {
                        return 0;
                    }
                    HookTool { kind: "read".into(), call_id, name: Some(tool_name.clone()), paths, failed, ..HookTool::default() }
                }
                _ => {
                    if !local.marked(&sid) {
                        return 0;
                    }
                    HookTool { kind: "other".into(), call_id, name: Some(tool_name.clone()), failed, ..HookTool::default() }
                }
            };
            let h = HookRequest { event: "post_tool".into(), tool: Some(tool), ..base };
            if let Ok(r) = contact(&local, &h, deadline) {
                context(&event, r.context);
            }
            0
        }
        "Stop" | "SubagentStop" | "SessionEnd" => {
            if !local.marked(&sid) {
                return 0;
            }
            let h = match event.as_str() {
                "Stop" => {
                    let running: Vec<String> = v
                        .get("background_tasks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|t| s(t, "status").is_none_or(|st| st == "running"))
                        .filter_map(|t| s(t, "id").map(str::to_string))
                        .collect();
                    HookRequest { event: "stop".into(), background_running: Some(running), ..base }
                }
                "SubagentStop" => HookRequest { event: "agent_stop".into(), ..base },
                _ => HookRequest { event: "session_end".into(), source: s(&v, "reason").map(str::to_string), ..base },
            };
            let _ = contact(&local, &h, deadline);
            0
        }
        _ => 0,
    }
}

/// `export TCAS_SESSION=…` into the file Claude Code sources before each
/// Bash command (harness.md section 2). Needs no tcasd.
fn write_env(sid: &str) {
    let Some(f) = std::env::var_os("CLAUDE_ENV_FILE").filter(|f| !f.is_empty()) else { return };
    let line = format!("export TCAS_SESSION={}\n", tcas_core::pathfmt::shell_quote(sid));
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn edit_paths(v: &Value, cwd: &str) -> Vec<String> {
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    ["file_path", "notebook_path"].iter().filter_map(|k| s(&input, k)).map(|p| abs(cwd, p)).collect()
}

/// Extra directories attached at Claude Code launch via `--add-dir <dirs...>`
/// or project `.claude/settings*.json` `permissions.additionalDirectories`
/// (plus optional `workspace_dirs` / `added_dirs` in JSON for testing).
fn startup_workspace_dirs(v: &Value, pid: i32, cwd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let expanded = if let Some(rest) = trimmed.strip_prefix("~/")
            && let Some(home) = tcas_sys::home::user_home()
        {
            home.join(rest).to_string_lossy().into_owned()
        } else {
            trimmed.to_string()
        };
        let resolved = abs(cwd, &expanded);
        if std::path::Path::new(&resolved).is_dir() && !out.contains(&resolved) {
            out.push(resolved);
        }
    };

    for key in ["added_dirs", "workspace_dirs"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            for item in arr.iter().filter_map(Value::as_str) {
                push(item);
            }
        }
    }

    // Parse `--add-dir <dirs...>` / `--add-dir=<dir>` from the harness process argv.
    let argv = tcas_sys::proc::args(pid);
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--" {
            break;
        }
        if let Some(val) = arg.strip_prefix("--add-dir=") {
            push(val);
            i += 1;
            continue;
        }
        if arg == "--add-dir" {
            i += 1;
            while i < argv.len() && !argv[i].starts_with('-') {
                push(&argv[i]);
                i += 1;
            }
            continue;
        }
        i += 1;
    }

    // Project-scoped `.claude/settings.json` and `.claude/settings.local.json`.
    let cwd_path = std::path::Path::new(cwd);
    for rel in [".claude/settings.json", ".claude/settings.local.json"] {
        if let Ok(text) = std::fs::read_to_string(cwd_path.join(rel))
            && let Ok(cfg) = serde_json::from_str::<Value>(&text)
            && let Some(arr) = cfg.get("permissions").and_then(|p| p.get("additionalDirectories")).and_then(Value::as_array)
        {
            for item in arr.iter().filter_map(Value::as_str) {
                push(item);
            }
        }
    }

    out
}

fn read_paths(v: &Value, cwd: &str) -> Vec<String> {
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = s(&input, "file_path") {
        out.push(abs(cwd, p));
    }
    if let Some(files) = v.get("tool_response").and_then(|r| r.get("filenames")).and_then(Value::as_array) {
        out.extend(files.iter().take(2000).filter_map(Value::as_str).map(|f| abs(cwd, f)));
    }
    out
}

/// Bash before it runs (harness.md sections 5.1, 5.2 and 5.5): `tcas`
/// commands are approved here; paths only the user may change are refused
/// here; the rest goes to tcasd (the guard and the reconcile window) when
/// the command concerns a workspace. Fail-closed like edits (section 7).
fn pre_bash(local: &Local, v: &Value, cwd: &str, sid: &str, base: HookRequest, deadline: Instant) {
    let input = v.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(command) = s(&input, "command") else { return };
    let background = input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
    let home = tcas_sys::home::user_home().map(|h| h.to_string_lossy().into_owned());
    let parsed = shell::parse(command, cwd, home.as_deref());
    let approve = local.ws_config(cwd).is_none_or(|c| c.bash.approve_tcas);

    if let Some(args) = guard::tcas_command(&parsed) {
        let words: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
        let is_wait = words.len() >= 2 && words[0] == "lease" && words[1] == "wait";
        let approved = approved(&words);
        if is_wait && !background {
            if approve {
                let mut updated = input.clone();
                updated["run_in_background"] = json!(true);
                decision("allow", "tcas lease wait runs in the background (tcas)", Some(updated));
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
    let dirs = bash_dirs(cwd, &parsed);
    let concerns = dirs.iter().any(|d| local.managed(d));
    if !(concerns || local.marked(sid)) {
        return;
    }
    let tool = HookTool {
        kind: "bash".into(),
        call_id: s(v, "tool_use_id").unwrap_or("").to_string(),
        name: Some("Bash".into()),
        command: Some(command.to_string()),
        background,
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

fn wait_in_background(command: &str) -> String {
    let one: String = command.lines().next().unwrap_or("").chars().take(200).collect();
    format!(
        "tcas: tcas lease wait can run for a long time and must not hold up this turn. Run it on its own, as a \
         background command (Bash tool, run_in_background), not inside a longer command line:\n  {one}"
    )
}
