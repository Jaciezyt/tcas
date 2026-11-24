//! Integration tests for `tcas plugin`, `tcas doctor`, and dead-process
//! liveness cleanup (Milestone 4 Phase B).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use serde_json::Value;

fn setup_fake_home(t: &T) -> std::path::PathBuf {
    let h = t.root.join("userhome");
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn write_exec(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn plugin_path_writes_skill_md() {
    let t = T::new();
    let uhome = setup_fake_home(&t);
    let o = t.cmd(None, &["plugin", "path", "claude"]).env("HOME", &uhome).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let dir = Path::new(std::str::from_utf8(&o.stdout).unwrap().trim());
    let skill = dir.join("skills/tcas/SKILL.md");
    assert!(skill.is_file(), "missing {}", skill.display());
    let text = std::fs::read_to_string(&skill).unwrap();
    assert!(text.contains("name: tcas"), "{text}");
    assert!(text.contains("tcas lease acquire"), "{text}");
    assert!(text.contains("tcas lease wait"), "{text}");
    assert!(dir.join("bin/tcas").is_file());
    assert!(dir.join("hooks/hooks.json").is_file());
    assert!(dir.join(".claude-plugin/plugin.json").is_file());
}

#[test]
fn plugin_install_refused_for_session_without_starting_daemon() {
    let t = T::new();
    let uhome = setup_fake_home(&t);
    let r = t.cmd(Some("claude:aaaa1111"), &["plugin", "install", "claude"])
        .env("HOME", &uhome)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(6), "{}", String::from_utf8_lossy(&r.stderr));
    assert!(String::from_utf8_lossy(&r.stderr).contains("only for the user"));
    assert!(t.pid().is_none(), "plugin install must not start tcasd");

    let rj = t.cmd(Some("claude:aaaa1111"), &["--json", "plugin", "install", "claude"])
        .env("HOME", &uhome)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(rj.status.code(), Some(6));
    let j: Value = serde_json::from_slice(&rj.stdout).unwrap();
    assert_eq!(j["error"]["code"], "user_only");
}

#[test]
fn plugin_install_writes_marketplace_and_invokes_claude() {
    let t = T::new();
    let uhome = setup_fake_home(&t);
    let bin = t.root.join("mockbin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls_log = t.root.join("claude_calls.log");
    let state_flag = t.root.join("claude_installed");

    let mock_claude = format!(
        r#"#!/bin/sh
echo "$@" >> "{calls}"
case "$*" in
  "plugin marketplace list --json")
    if [ -f "{flag}" ]; then
      printf '[{{"name":"tcas","source":"directory","path":"{mkt}"}}]\n'
    else
      printf '[]\n'
    fi
    ;;
  "plugin list --json")
    if [ -f "{flag}" ]; then
      printf '[{{"id":"tcas@tcas","version":"0.1.0","enabled":false}}]\n'
    else
      printf '[]\n'
    fi
    ;;
  "plugin install tcas@tcas")
    touch "{flag}"
    ;;
  *)
    ;;
esac
exit 0
"#,
        calls = calls_log.display(),
        flag = state_flag.display(),
        mkt = t.home.join("plugins").display(),
    );
    write_exec(&bin.join("claude"), &mock_claude);
    let path_env = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());

    let out = t.cmd(None, &["--json", "plugin", "install", "claude"])
        .env("HOME", &uhome)
        .env("PATH", &path_env)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let j: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["action"], "installed");
    assert_eq!(j["harness"], "claude");

    let mkt_json = t.home.join("plugins/.claude-plugin/marketplace.json");
    assert!(mkt_json.is_file());
    let mj: Value = serde_json::from_str(&std::fs::read_to_string(&mkt_json).unwrap()).unwrap();
    assert_eq!(mj["name"], "tcas");
    assert_eq!(mj["plugins"][0]["name"], "tcas");
    assert_eq!(mj["plugins"][0]["source"], "./claude");
    assert!(t.home.join("plugins/claude/skills/tcas/SKILL.md").is_file());

    let calls = std::fs::read_to_string(&calls_log).unwrap();
    assert!(calls.contains("plugin marketplace add "), "{calls}");
    assert!(calls.contains("plugin install tcas@tcas"), "{calls}");

    // Second run: marketplace matches -> update, plugin disabled -> enable.
    let out2 = t.cmd(None, &["plugin", "install", "claude"])
        .env("HOME", &uhome)
        .env("PATH", &path_env)
        .output()
        .unwrap();
    assert!(out2.status.success(), "{}", String::from_utf8_lossy(&out2.stderr));
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains("updated claude plugin tcas@tcas"), "{s2}");
    let calls2 = std::fs::read_to_string(&calls_log).unwrap();
    assert!(calls2.contains("plugin marketplace update tcas"), "{calls2}");
    assert!(calls2.contains("plugin update tcas@tcas"), "{calls2}");
    assert!(calls2.contains("plugin enable tcas@tcas"), "{calls2}");
}

#[test]
fn doctor_stopped_running_malformed_toml_and_old_git() {
    let t = T::new();
    let uhome = setup_fake_home(&t);
    let app = t.repo("app", &[("a.txt", "a\n")]);

    // 1. Stopped daemon: must NOT auto-start tcasd.
    let stopped_json = t.cmd(None, &["--json", "doctor"])
        .current_dir(&app)
        .env("HOME", &uhome)
        .output()
        .unwrap();
    assert!(stopped_json.status.success(), "{}", String::from_utf8_lossy(&stopped_json.stderr));
    assert!(t.pid().is_none(), "doctor must not auto-start tcasd");
    let dj: Value = serde_json::from_slice(&stopped_json.stdout).unwrap();
    assert_eq!(dj["tcasd"]["running"], false);
    assert!(dj["workspace"].is_null());
    assert_eq!(dj["git"]["config_hooks_supported"], true);

    let stopped_human = t.cmd(None, &["doctor"])
        .current_dir(&app)
        .env("HOME", &uhome)
        .output()
        .unwrap();
    assert!(stopped_human.status.success());
    let h = String::from_utf8_lossy(&stopped_human.stdout);
    assert!(h.contains("tcasd      not running"), "{h}");
    assert!(h.contains("exempt directories:"), "{h}");

    // 2. Running daemon in workspace with malformed .tcas.toml.
    std::fs::write(app.join(".tcas.toml"), "mode = [broken\n").unwrap();
    t.cmd(None, &["workspace", "open"]).current_dir(&app).env("HOME", &uhome).output().unwrap();

    let run_json = t.cmd(None, &["--json", "doctor"])
        .current_dir(&app)
        .env("HOME", &uhome)
        .output()
        .unwrap();
    assert!(run_json.status.success());
    let rj: Value = serde_json::from_slice(&run_json.stdout).unwrap();
    assert_eq!(rj["tcasd"]["running"], true);
    assert_eq!(rj["tcasd"]["version_mismatch"], false);
    assert_eq!(rj["workspace"]["name"], "app");
    assert_eq!(rj["workspace"]["repos"][0]["hooks_installed"], true);
    let problems = rj["problems"].as_array().unwrap();
    assert!(!problems.is_empty(), "expected parse problem for malformed .tcas.toml: {rj}");
    assert!(problems.iter().any(|p| p.as_str().unwrap_or("").contains(".tcas.toml")), "{rj}");

    let run_human = t.cmd(None, &["doctor"])
        .current_dir(&app)
        .env("HOME", &uhome)
        .output()
        .unwrap();
    assert!(run_human.status.success());
    let rh = String::from_utf8_lossy(&run_human.stdout);
    assert!(rh.contains("tcasd      running"), "{rh}");
    assert!(rh.contains("workspace  \"app\""), "{rh}");
    assert!(rh.contains("hooks installed"), "{rh}");
    assert!(rh.contains("problems:"), "{rh}");

    // 3. Shim git on PATH that does not support `git hook run`.
    let shim_dir = t.root.join("shimgit");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let real_git = Command::new("which").arg("git").output().unwrap();
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_string();
    let shim_script = format!(
        r#"#!/bin/sh
for a in "$@"; do
  if [ "$a" = "hook" ]; then
    echo "git: 'hook' is not a git command." >&2
    exit 1
  fi
done
exec "{real_git}" "$@"
"#
    );
    write_exec(&shim_dir.join("git"), &shim_script);
    let path_shim = format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap_or_default());

    let shim_out = t.cmd(None, &["--json", "doctor"])
        .current_dir(&app)
        .env("HOME", &uhome)
        .env("PATH", &path_shim)
        .output()
        .unwrap();
    assert!(shim_out.status.success());
    let sj: Value = serde_json::from_slice(&shim_out.stdout).unwrap();
    assert_eq!(sj["git"]["config_hooks_supported"], false, "{sj}");
}

#[test]
fn liveness_sigkill_cleans_up_clean_leases_and_orphans_dirty() {
    let t = T::new();
    std::fs::write(
        t.home.join("config.toml"),
        "[daemon]\nliveness_interval_secs = 1\n[exempt]\nbuiltin = false\n",
    )
    .unwrap();
    let app = t.repo("app", &[("a.txt", "a\n"), ("src/x.ts", "x\n")]);
    t.ok(None, &["workspace", "open", app.to_str().unwrap()]);

    let mut s = S::at(&t, "dead1111-2222", &app);
    s.start("startup");
    // Explicit clean dir lease on src/, implicit dirty edit on a.txt.
    assert_eq!(s.tcas(&["lease", "acquire", "src/", "--brief", "work on src"]).code, 0);
    assert!(!s.edit(&app.join("a.txt"), "dirty\n").denied());

    // Simulate harness SIGKILL (no SessionEnd hook).
    let _ = s.proc.kill();
    let _ = s.proc.wait();

    t.until("liveness cleanup ends dead session", || {
        let sessions = t.ok(None, &["--json", "session", "list", "--all"]).json();
        sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == s.sid() && x["state"] == "ended")
    });

    // Wait briefly if needed, though end_session already ran in the same liveness tick.
    std::thread::sleep(Duration::from_millis(50));
    let ls = leases(&t);
    assert!(!ls.iter().any(|l| l.1 == "src/"), "clean dir lease must be released: {ls:?}");
    assert!(
        ls.iter().any(|l| l.0 == s.sid() && l.1 == "a.txt" && l.3 == "orphaned"),
        "dirty file lease must become orphaned: {ls:?}"
    );
}
