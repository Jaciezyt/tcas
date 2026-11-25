//! Acceptance cases from docs/v1/plan.md section 4 that the other test
//! files did not cover. Each test names its cases.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use serde_json::json;

fn commit_toml(app: &Path, toml: &str) {
    std::fs::write(app.join(".tcas.toml"), toml).unwrap();
    git(app, &["add", ".tcas.toml"]);
    commit(app, &[".tcas.toml"]);
}

/// Runs `tcas` as the user from `dir`.
fn user_in(t: &T, dir: &Path, args: &[&str]) -> Run {
    let o = t.cmd(None, args).current_dir(dir).output().unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// Case 37: an expired directory lease yields, the dirty file under it stays.
#[test]
fn case_37_expired_directory_yields_but_dirty_file_stays() {
    let t = T::new();
    let app = one_repo(&t);
    commit_toml(&app, "[lease]\nexplicit_idle_secs = 1\n");
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/"]).code, 0);
    assert!(!a.edit(&app.join("src/auth/x.ts"), "A\n").denied());
    std::thread::sleep(Duration::from_millis(1500));

    let r = b.edit(&app.join("src/auth/y.ts"), "B\n");
    assert!(!r.denied(), "{}", r.v);
    let held = leases(&t);
    assert!(!held.iter().any(|l| l.0.contains("aaaa") && l.1 == "src/auth/"), "{held:?}");
    assert!(held.iter().any(|l| l.0.contains("aaaa") && l.1 == "src/auth/x.ts"), "{held:?}");
    assert!(b.pre_edit(&app.join("src/auth/x.ts")).denied());
    let c = a.prompt("next").context();
    assert!(c.contains("src/auth/"), "A is told: {c}");
}

/// Case 39: the directory size limit, and the user's grant past it.
#[test]
fn case_39_directory_size_limit_and_grant() {
    let t = T::new();
    let app = t.repo("app", &[("src/auth/a.ts", "a\n"), ("src/auth/b.ts", "b\n"), ("src/auth/c.ts", "c\n")]);
    commit_toml(&app, "[lease]\nmax_dir_files = 2\n");
    assert_eq!(user_in(&t, &app, &["workspace", "open"]).code, 0);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    // Refused with exit code 1 and the file count (daemon-and-cli.md section 5).
    let r = a.tcas(&["lease", "acquire", "src/auth/"]);
    assert_eq!(r.code, 1, "{}{}", r.out, r.err);
    assert!(r.out.contains("3 tracked files"), "the file count: {}{}", r.out, r.err);
    let r = user_in(&t, &app, &["lease", "grant", &a.sid(), "src/auth/"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(leases(&t).iter().any(|l| l.1 == "src/auth/" && l.2 == "grant"));
}

/// Case 48: opening a directory around an open workspace is refused.
#[test]
fn case_48_overlapping_workspace_is_refused() {
    let t = T::new();
    let web = t.repo("web", &[("a.txt", "a\n")]);
    assert_eq!(user_in(&t, &web, &["workspace", "open"]).code, 0);
    let r = t.run(None, &["workspace", "open"]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("around") && r.err.contains("web"), "{}", r.err);
}

/// Cases 69, 70: two sessions in one worktree exclude each other; the main
/// checkout's file of the same name is another file.
#[test]
fn case_69_70_worktree_is_its_own_checkout() {
    let t = T::new();
    let app = t.repo("app", &[("src/a.ts", "a\n"), (".gitignore", ".claude/\n")]);
    assert_eq!(user_in(&t, &app, &["workspace", "open"]).code, 0);
    let wt = app.join(".claude/worktrees/x");
    git(&app, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let (a, b, c) = (S::at(&t, "aaaa1111-2222", &wt), S::at(&t, "bbbb3333-4444", &wt), S::at(&t, "cccc5555-6666", &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }
    assert!(!a.edit(&wt.join("src/a.ts"), "A\n").denied());
    assert!(leases(&t).iter().any(|l| l.1 == ".claude/worktrees/x/src/a.ts"), "{:?}", leases(&t));
    assert!(b.pre_edit(&wt.join("src/a.ts")).denied());
    assert!(!c.edit(&app.join("src/a.ts"), "C\n").denied());
}

/// Case 72: a worktree outside the workspace is listed on open and is free
/// until added; afterwards sessions in it exclude each other.
#[test]
fn case_72_outside_worktree_is_listed_then_added() {
    let t = T::new();
    let app = t.repo("app", &[("a.txt", "a\n")]);
    let wt = t.proj.join("app-x");
    git(&app, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let r = user_in(&t, &app, &["workspace", "open"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("app-x") && r.out.contains("tcas workspace add"), "{}", r.out);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &wt), S::at(&t, "bbbb3333-4444", &wt));
    a.start("startup");
    b.start("startup");
    assert!(!a.edit(&wt.join("a.txt"), "A\n").denied());
    assert!(!b.pre_edit(&wt.join("a.txt")).denied(), "not managed yet");
    assert_eq!(user_in(&t, &app, &["workspace", "add", wt.to_str().unwrap()]).code, 0);
    assert!(!a.edit(&wt.join("a.txt"), "A2\n").denied());
    assert!(b.pre_edit(&wt.join("a.txt")).denied());
}

/// Case 78: what a session may not open.
#[test]
fn case_78_session_open_limits() {
    let t = T::new();
    let uhome = t.root.join("uhome");
    std::fs::create_dir_all(&uhome).unwrap();
    let many = t.proj.join("many");
    for i in 0..9 {
        let d = many.join(format!("r{i}"));
        std::fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q"]);
    }
    let app = t.repo("app", &[("a.txt", "a\n")]);
    let sid = "claude:aaaa1111-2222";
    let run = |dir: &Path, args: &[&str]| {
        let o = t.cmd(Some(sid), args).current_dir(dir).env("HOME", &uhome).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let (code, err) = run(&uhome, &["workspace", "open", uhome.to_str().unwrap()]);
    assert_eq!(code, 6, "{err}");
    let (code, err) = run(&many, &["workspace", "open"]);
    assert_eq!(code, 6, "{err}");
    let (code, err) = run(&app, &["workspace", "open", "--strict"]);
    assert_eq!(code, 6, "{err}");
    assert!(t.ok(None, &["--json", "workspace", "list"]).json().as_array().unwrap().is_empty());
}

/// Cases 80, 81: user-only commands, also with the session variables
/// removed (the session is found among the command's ancestors).
#[test]
fn case_80_81_user_only_commands() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    assert_eq!(a.tcas(&["workspace", "close"]).code, 6);
    assert_eq!(a.tcas(&["workspace", "set", "mode", "auto"]).code, 6);

    // A session whose harness process is this test: every command it starts
    // has it among its ancestors.
    let me = std::process::id().to_string();
    let h = S::at(&t, "hhhh7777-8888", &app);
    h.hook_env(&mut json!({ "hook_event_name": "SessionStart", "source": "startup" }), &[("TCAS_HOOK_PID", me.as_str())]);
    let r = user_in(&t, &app, &["lease", "release", "--force", "a.txt"]);
    assert_eq!(r.code, 6, "{}{}", r.out, r.err);
    let who = user_in(&t, &app, &["--json", "whoami"]).json();
    assert_eq!(who["caller"], "claude:hhhh7777-8888", "{who}");
}

/// Case 82: a workspace whose root worktree is removed closes itself.
#[test]
fn case_82_removed_root_worktree_closes_the_workspace() {
    let t = T::new();
    let app = t.repo("app", &[("a.txt", "a\n")]);
    let wt = t.proj.join("app-x");
    git(&app, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    assert_eq!(user_in(&t, &wt, &["workspace", "open"]).code, 0);
    git(&app, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
    t.until("the workspace to close", || t.ok(None, &["--json", "workspace", "list"]).json().as_array().unwrap().is_empty());
    let closed = events(&t).into_iter().find(|e| e["type"] == "workspace.closed").expect("workspace.closed");
    assert!(closed.to_string().contains("gone") || closed.to_string().contains("missing"), "{closed}");
}

/// Case 86: one request across two roots is all or nothing.
#[test]
fn case_86_acquire_across_roots_is_all_or_nothing() {
    let t = T::new();
    let app = t.repo("app", &[("src/auth/a.ts", "a\n")]);
    let backend = t.repo("app-backend", &[("src/api/user.ts", "u\n")]);
    assert_eq!(user_in(&t, &app, &["workspace", "open"]).code, 0);
    assert_eq!(user_in(&t, &app, &["workspace", "add", backend.to_str().unwrap()]).code, 0);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert_eq!(b.tcas(&["lease", "acquire", "../app-backend/src/api/user.ts"]).code, 0);
    let r = a.tcas(&["lease", "acquire", "src/auth/", "../app-backend/src/api/"]);
    assert_eq!(r.code, 3, "{}{}", r.out, r.err);
    assert!(!leases(&t).iter().any(|l| l.0.contains("aaaa")), "{:?}", leases(&t));
}

/// Case 90: roots cannot nest; a session cannot remove the user's root.
#[test]
fn case_90_nested_roots_and_removing_the_users_root() {
    let t = T::new();
    let app = t.repo("app", &[("a.txt", "a\n")]);
    let backend = t.repo("app-backend", &[("b.txt", "b\n")]);
    assert_eq!(user_in(&t, &app, &["workspace", "open"]).code, 0);
    std::fs::create_dir_all(app.join("sub")).unwrap();
    let r = user_in(&t, &app, &["workspace", "add", app.join("sub").to_str().unwrap()]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("inside") || r.err.contains("checkout"), "{}", r.err);
    assert_eq!(user_in(&t, &app, &["workspace", "add", backend.to_str().unwrap()]).code, 0);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    assert_eq!(a.tcas(&["workspace", "remove", "../app-backend"]).code, 6);
}

/// Case 94: exempt directories cannot be opened, by the user or a session,
/// under either spelling of the temporary directory.
#[test]
fn case_94_exempt_directories_cannot_be_opened() {
    let t = T::new();
    std::fs::write(t.home.join("config.toml"), "[exempt]\nbuiltin = true\n").unwrap();
    let scratch = t.repo("scratch-repo", &[("a.txt", "a\n")]);
    let short = PathBuf::from(scratch.to_string_lossy().replacen("/private/tmp/", "/tmp/", 1));
    for p in [&scratch, &short] {
        let r = t.run(None, &["workspace", "open", p.to_str().unwrap()]);
        assert_ne!(r.code, 0, "{}", r.out);
        assert!(r.err.contains("exempt"), "{}", r.err);
        let r = t.run(Some("claude:aaaa1111-2222"), &["workspace", "open", p.to_str().unwrap()]);
        assert_ne!(r.code, 0, "{}", r.out);
    }
}

/// Case 98: a tracked file under an ignored directory is managed.
#[test]
fn case_98_force_added_file_is_managed() {
    let t = T::new();
    let app = one_repo(&t);
    std::fs::create_dir_all(app.join("dist")).unwrap();
    std::fs::write(app.join("dist/keep.js"), "k\n").unwrap();
    git(&app, &["add", "-f", "dist/keep.js"]);
    commit(&app, &["dist/keep.js"]);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert!(!a.edit(&app.join("dist/keep.js"), "A\n").denied());
    assert!(b.pre_edit(&app.join("dist/keep.js")).denied());
}

/// Case 102: files in the git directory are not managed.
#[test]
fn case_102_git_directory_is_not_managed() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    for f in [".git/info/exclude", ".git/config"] {
        assert!(!a.pre_edit(&app.join(f)).denied(), "{f}");
        assert!(!b.pre_edit(&app.join(f)).denied(), "{f}");
    }
    assert!(leases(&t).is_empty(), "{:?}", leases(&t));
}

/// Case 103: a directory inside a checkout cannot be a root. (Without a
/// path, `open` takes the checkout's root instead.)
#[test]
fn case_103_open_inside_a_checkout_names_the_checkout() {
    let t = T::new();
    t.repo("web", &[("src/a.ts", "a\n")]);
    let r = t.run(None, &["workspace", "open", "web/src"]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("inside the checkout") && r.err.contains("web"), "{}", r.err);
}
