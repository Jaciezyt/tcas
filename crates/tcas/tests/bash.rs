//! End-to-end checks for milestone 3 (plan.md section 3): the command
//! guard, reconciling Bash calls, background windows, `tcas commit` and
//! git's hooks, against a real tcasd.

mod common;

use std::path::Path;

use serde_json::Value;

use common::*;

const A: &str = "aaaa1111-2222";
const B: &str = "bbbb3333-4444";
const C: &str = "cccc5555-6666";

/// Commits made in tests: no signing, whatever the user's git does.
fn no_sign(dir: &Path) {
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
}

fn app(t: &T) -> std::path::PathBuf {
    let app = one_repo(t);
    no_sign(&app);
    app
}

/// `app` with a `.tcas.toml` (read again when it changes).
fn app_with(t: &T, toml: &str) -> std::path::PathBuf {
    let d = app(t);
    std::fs::write(d.join(".tcas.toml"), toml).unwrap();
    d
}

/// A repository whose `.claude/` is excluded, with a linked worktree in
/// `.claude/worktrees/x`, opened.
fn with_inner_worktree(t: &T) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n"), (".gitignore", ".claude/\n")]);
    no_sign(&d);
    git(&d, &["worktree", "add", "-q", "-b", "x", ".claude/worktrees/x"]);
    let o = t.cmd(None, &["workspace", "open"]).current_dir(&d).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let wt = d.join(".claude/worktrees/x");
    (d, wt)
}

fn has_event(t: &T, kind: &str, f: impl Fn(&Value) -> bool) -> bool {
    events(t).iter().any(|e| e["type"] == kind && f(e))
}

fn held_by(t: &T, sid: &str) -> Vec<(String, String)> {
    leases(t).into_iter().filter(|l| l.0 == format!("claude:{sid}")).map(|l| (l.1, l.2)).collect()
}

fn head_files(dir: &Path) -> Vec<String> {
    git_text(dir, &["show", "--name-only", "--format=", "HEAD"]).lines().map(str::to_string).collect()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

// ---------------------------------------------------------------------------
// Reconciling

/// Case 9: a file written by Bash, unheld, nobody else running Bash.
#[test]
fn bash_writes_take_leases() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    let r = a.bash("echo c > c.txt");
    assert!(!r.denied(), "{}", r.reason());
    assert_eq!(held_by(&t, A), [("c.txt".into(), "bash".into())]);
}

/// Case 10: Bash writes a file another session holds.
#[test]
fn overreach_is_reported_to_both() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    b.edit(&app.join("a.txt"), "B\n");

    let r = a.bash("echo overreach > a.txt");
    assert_eq!(read(&app.join("a.txt")), "overreach\n", "Bash cannot be stopped");
    assert!(r.context().contains("changed files that other sessions hold"), "{}", r.context());
    assert!(r.context().contains("a.txt"), "{}", r.context());
    assert!(has_event(&t, "bash.overreach", |_| true));
    assert_eq!(held_by(&t, B), [("a.txt".into(), "edit".into())]);

    let c = b.post_other().context();
    assert!(c.contains("changed files you hold with a Bash command"), "{c}");
    assert!(c.contains("echo overreach > a.txt"), "{c}");
}

/// Case 27: a session waiting does not make attribution uncertain.
#[test]
fn a_wait_is_not_a_bash_window() {
    let t = T::new();
    let app = app(&t);
    let (a, b, c) = (S::at(&t, A, &app), S::at(&t, B, &app), S::at(&t, C, &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }
    a.edit(&app.join("a.txt"), "A\n");
    let (_, waiter) = b.bash_bg("tcas lease wait a.txt --timeout 30", "bwait");
    let r = c.bash("echo c > c.txt");
    assert!(r.context().is_empty(), "{}", r.context());
    assert_eq!(held_by(&t, C), [("c.txt".into(), "bash".into())]);
    drop(waiter.map(|mut w| w.kill()));
}

/// Unheld files changed while another session runs Bash are not given to
/// either; they are listed as unattributed.
#[test]
fn concurrent_bash_leaves_files_unattributed() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    // B's command is still running while A's runs.
    assert!(!b.pre_bash("sleep 1", false).denied());
    let r = a.bash("echo u > u.txt");
    assert!(r.context().contains("not attributed to you"), "{}", r.context());
    assert!(held_by(&t, A).is_empty());
    let s = a.tcas(&["status"]).out;
    assert!(s.contains("u.txt"), "{s}");
}

/// Case 45: from the root, Bash writes into a nested repository.
#[test]
fn writes_in_another_repository_of_the_workspace() {
    let t = T::new();
    two_repos(&t);
    for r in ["api", "web"] {
        no_sign(&t.proj.join(r));
    }
    let a = S::new(&t, A);
    a.start("startup");
    a.bash("echo c > api/c.txt");
    assert_eq!(held_by(&t, A), [("api/c.txt".into(), "bash".into())]);
}

/// Case 14: a background command writes after its PostToolUse; the next
/// reconcile gives A the file.
#[test]
fn background_commands_are_reconciled() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    let (_, child) = a.bash_bg("sleep 1; echo bg > bg.txt", "task1");
    assert!(held_by(&t, A).is_empty());
    let mut child = child.unwrap();
    child.wait().unwrap();
    assert_eq!(read(&app.join("bg.txt")), "bg\n");
    // The window closes when Stop no longer lists the task.
    a.stop_with(&[]);
    assert_eq!(held_by(&t, A), [("bg.txt".into(), "bash".into())]);
}

/// Each Bash call of a session with a background window open reconciles
/// that window too.
#[test]
fn background_windows_roll_forward() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    let (_, child) = a.bash_bg("sleep 1; echo bg > bg.txt", "task1");
    child.unwrap().wait().unwrap();
    a.bash("true");
    assert_eq!(held_by(&t, A), [("bg.txt".into(), "bash".into())]);
}

/// Case 15: the branch changes during A's Bash call; B, holding a lease
/// in the checkout, is told.
#[test]
fn branch_changes_are_reported() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    git(&app, &["branch", "other"]);
    let r = a.bash("git switch -q other");
    assert!(!r.denied(), "{}", r.reason());
    assert!(has_event(&t, "branch.changed", |e| e["payload"]["to"] == "other"), "{:?}", events(&t));

    // With B holding a lease, a switch is refused by the guard; one made
    // outside every window (the user) is still noticed.
    b.edit(&app.join("b.txt"), "B\n");
    assert!(a.bash("git switch -q main").denied());
    git(&app, &["switch", "-q", "main"]);
    a.bash("true");
    let c = b.post_other().context();
    assert!(c.contains("the branch of") && c.contains("to main"), "{c}");

    // A script the guard cannot read: B is told once, with A as the cause.
    let sw = t.root.join("sw.sh");
    std::fs::write(&sw, "git switch -q other\n").unwrap();
    let r = a.bash(&format!("sh {}", sw.display()));
    assert_eq!(r.code(), 0, "{}", r.output());
    let c = b.post_other().context();
    assert_eq!(c.matches("the branch of").count(), 1, "{c}");
    assert!(c.contains("by claude:aaaa…") && c.contains("to other"), "{c}");

    // `tcas status` records `branch_changed_at` and shows `(branch changed ... ago)`.
    let st = a.tcas(&["--json", "status"]).json();
    assert!(st["workspace"]["repos"][0]["branch_changed_at"].is_i64(), "{st}");
    let txt = a.tcas(&["status"]).out;
    assert!(txt.contains("(branch changed ") && txt.contains("ago)"), "{txt}");
}

/// Case 101: a file given by reconciling, then excluded by git: B may edit
/// it and A's lease is released, including via `tcas status` and `tcas lease acquire`.
#[test]
fn excluded_after_the_fact() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    std::fs::create_dir_all(app.join("src")).unwrap();
    a.bash("echo n > src/new.ts");
    assert_eq!(held_by(&t, A), [("src/new.ts".into(), "bash".into())]);
    std::fs::write(app.join(".git/info/exclude"), "src/new.ts\n").unwrap();
    let r = b.edit(&app.join("src/new.ts"), "B\n");
    assert!(!r.denied(), "{}", r.reason());
    assert!(held_by(&t, A).is_empty());

    // Excluding a leased file also releases it upon `tcas status`.
    a.bash("echo s > src/status_ex.ts");
    assert_eq!(held_by(&t, A), [("src/status_ex.ts".into(), "bash".into())]);
    std::fs::write(app.join(".git/info/exclude"), "src/new.ts\nsrc/status_ex.ts\n").unwrap();
    let st = b.tcas(&["--json", "status"]).json();
    let released: Vec<&str> = st["released"].as_array().unwrap().iter().filter_map(|p| p["display"].as_str()).collect();
    assert!(released.contains(&"src/status_ex.ts"), "{st}");
    assert!(held_by(&t, A).is_empty(), "{:?}", leases(&t));
    assert!(has_event(&t, "lease.released", |e| e["payload"]["path"] == "src/status_ex.ts" && e["payload"]["reason"] == "excluded"));

    // Excluding a leased file also releases it upon `tcas lease acquire`.
    a.bash("echo q > src/acq_ex.ts");
    assert_eq!(held_by(&t, A), [("src/acq_ex.ts".into(), "bash".into())]);
    std::fs::write(app.join(".git/info/exclude"), "src/new.ts\nsrc/status_ex.ts\nsrc/acq_ex.ts\n").unwrap();
    let acq = b.tcas(&["lease", "acquire", "src/acq_ex.ts"]);
    assert_eq!(acq.code, 0, "{}\n{}", acq.out, acq.err);
    assert!(acq.out.contains("excluded by git"), "{}", acq.out);
    assert!(held_by(&t, A).is_empty(), "{:?}", leases(&t));
    assert!(has_event(&t, "lease.released", |e| e["payload"]["path"] == "src/acq_ex.ts" && e["payload"]["reason"] == "excluded"));
}

/// Case 104: a background command past its first minute no longer counts
/// as running: B's new file is B's, and A's clean implicit lease yields.
#[test]
fn long_background_commands_stop_counting() {
    let t = T::new();
    let app = app_with(&t, "[bash]\nbackground_window_secs = 1\n");
    let (a, b, c) = (S::at(&t, A, &app), S::at(&t, B, &app), S::at(&t, C, &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }
    // A has an implicit lease on a.txt, committed and clean.
    a.edit(&app.join("a.txt"), "A\n");
    assert_eq!(a.tcas(&["commit", "-m", "a"]).code, 0);
    a.edit(&app.join("b.txt"), "A2\n");
    git(&app, &["checkout", "--", "b.txt"]);
    let (_, child) = a.bash_bg("sleep 30", "server");
    std::thread::sleep(std::time::Duration::from_millis(1300));

    b.bash("echo b > new.txt");
    assert!(held_by(&t, B).contains(&("new.txt".into(), "bash".into())), "{:?}", leases(&t));
    let r = c.edit(&app.join("b.txt"), "C\n");
    assert!(!r.denied(), "{}", r.reason());
    assert!(!held_by(&t, A).iter().any(|l| l.0 == "b.txt"));
    let mut child = child.unwrap();
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// The command guard

/// Cases 11, 12: `git add -A` is refused only when others hold leases.
#[test]
fn add_all_only_alone() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    let r = a.bash("git add -A");
    assert!(!r.denied(), "{}", r.reason());
    git(&app, &["reset", "-q"]);

    b.edit(&app.join("b.txt"), "B\n");
    let r = a.bash("git add -A");
    assert!(r.denied());
    assert!(r.reason().contains("tcas commit"), "{}", r.reason());
    assert!(git_text(&app, &["diff", "--cached", "--name-only"]).is_empty());
    assert!(has_event(&t, "bash.blocked", |e| e["payload"]["rule"] == "add_all"));
}

/// Cases 47, 49 to 54 and 89: which git commands are refused, in which
/// checkout, however they are written.
#[test]
fn guard_rules_by_checkout() {
    let t = T::new();
    two_repos(&t);
    let (api, web) = (t.proj.join("api"), t.proj.join("web"));
    for r in [&api, &web] {
        no_sign(r);
    }
    git(&api, &["config", "alias.ca", "commit -a"]);
    std::fs::write(api.join("src/mine.ts"), "mine\n").unwrap();
    git(&api, &["add", "src/mine.ts"]);
    git(&api, &["commit", "-qm", "add mine"]);
    let (a, b) = (S::new(&t, A), S::new(&t, B));
    a.start("startup");
    b.start("startup");
    b.edit(&api.join("src/app.ts"), "B\n");
    a.edit(&api.join("src/mine.ts"), "A\n");

    let refused = [
        "git -C api add -A",
        "cd api && git stash",
        "(cd web && git status); git -C api add -A",
        "bash -c \"cd api && git commit -am x\"",
        "env FOO=1 git -C api stash",
        "/usr/bin/git -C api reset --hard",
        "git -C api $SUB",
        "ls | xargs git -C api add",
        "git -C api ca -m x",
    ];
    for cmd in refused {
        let r = a.pre_bash(cmd, false);
        assert!(r.denied(), "should be refused: {cmd}");
    }
    let r = a.pre_bash("cd api && git stash", false);
    assert!(r.reason().contains("in api/"), "{}", r.reason());
    let r = a.pre_bash("git -C api $SUB", false);
    assert!(r.reason().contains("explicitly"), "{}", r.reason());

    let allowed = ["git -C web add -A", "git -C api status", "git -C api diff", "git -C api log -1"];
    for cmd in allowed {
        let r = a.pre_bash(cmd, false);
        assert!(!r.denied(), "should be allowed: {cmd}: {}", r.reason());
    }

    // Case 52: a quoted `;` is not a separator; the file is A's own.
    let r = a.bash("git -C api commit -q -m \"fix; cleanup\" -- src/mine.ts");
    assert!(!r.denied(), "{}", r.reason());
    assert_eq!(r.code(), 0, "{}", r.output());
    assert_eq!(head_files(&api), ["src/mine.ts"]);
}

/// Case 89: a second root of the workspace is guarded the same way.
#[test]
fn guard_in_a_second_root() {
    let t = T::new();
    let app = app(&t);
    let be = t.proj.join("app-backend");
    std::fs::create_dir_all(&be).unwrap();
    git(&be, &["init", "-q", "-b", "main"]);
    no_sign(&be);
    std::fs::create_dir_all(be.join("src")).unwrap();
    std::fs::write(be.join("src/a.ts"), "a\n").unwrap();
    git(&be, &["add", "-A"]);
    git(&be, &["commit", "-qm", "init"]);
    let r = t.cmd(None, &["workspace", "add", "../app-backend"]).current_dir(&app).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));

    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");

    // Case 84: hooks installed; displays relative to the session.
    assert!(git_text(&be, &["config", "--local", "--get-all", "include.path"]).contains("git-hooks.gitconfig"));
    a.edit(&be.join("src/a.ts"), "A\n");
    assert_eq!(held_by(&t, A), [("app-backend/src/a.ts".into(), "edit".into())]);
    let r = b.pre_edit(&be.join("src/a.ts"));
    assert!(r.denied());
    assert!(r.reason().contains("tcas lease wait ../app-backend/src/a.ts"), "{}", r.reason());

    // Case 85: Bash from the main root into the second.
    a.bash("echo x > ../app-backend/c.txt");
    assert!(held_by(&t, A).contains(&("app-backend/c.txt".into(), "bash".into())), "{:?}", leases(&t));

    // Case 89.
    let r = b.pre_bash("cd ../app-backend && git stash", false);
    assert!(r.denied(), "{}", r.v);

    // Case 87: one commit per root.
    a.edit(&app.join("a.txt"), "A\n");
    let c = a.tcas(&["commit", "-m", "both"]);
    assert_eq!(c.code, 0, "{}\n{}", c.out, c.err);
    assert_eq!(head_files(&app), ["a.txt"]);
    assert_eq!(head_files(&be), ["c.txt", "src/a.ts"]);
}

/// Case 96: tcas's own files and `.tcas.toml`, by editing tool, by Bash,
/// and by `tcas exempt add`.
#[test]
fn config_files_by_every_route() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    let home = t.home.display().to_string();
    assert!(a.pre_edit(&t.home.join("config.toml")).denied());
    assert!(a.pre_edit(&app.join(".tcas.toml")).denied());
    assert!(a.pre_bash(&format!("echo x >> {home}/config.toml"), false).denied());
    assert!(a.pre_bash("echo x >> .tcas.toml", false).denied());
    let r = a.tcas(&["exempt", "add", "/tmp/elsewhere"]);
    assert_ne!(r.code, 0, "{}", r.out);
    // Reading them is fine.
    assert!(!a.pre_bash("cat .tcas.toml", false).denied());
}

// ---------------------------------------------------------------------------
// tcas commit

/// Case 13: only A's file goes in; what B staged stays staged.
#[test]
fn commit_takes_only_own_files() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    b.edit(&app.join("b.txt"), "B\n");
    assert!(!b.bash("git add b.txt").denied());
    let r = a.tcas(&["commit", "-m", "x"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert_eq!(head_files(&app), ["a.txt"]);
    assert_eq!(git_text(&app, &["diff", "--cached", "--name-only"]).trim(), "b.txt");
    assert!(held_by(&t, A).is_empty(), "{:?}", leases(&t));
    assert!(r.out.contains("released: 1 file lease."), "{}", r.out);
}

/// Case 35: a directory lease stays; only the file A changed goes in.
#[test]
fn commit_under_a_directory_lease() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/"]).code, 0);
    a.edit(&app.join("src/auth/x.ts"), "A\n");
    std::fs::write(app.join("src/auth/y.ts"), "someone else\n").unwrap();
    let before = held_by(&t, A);
    assert!(before.contains(&("src/auth/".into(), "cli".into())), "{before:?}");
    assert!(before.contains(&("src/auth/x.ts".into(), "edit".into())), "{before:?}");
    let r = a.tcas(&["commit", "-m", "x"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert_eq!(head_files(&app), ["src/auth/x.ts"]);
    assert_eq!(held_by(&t, A), [("src/auth/".into(), "cli".into())]);
    assert!(r.out.contains("still held: src/auth/ (directory)"), "{}", r.out);
}

/// Cases 43, 44: one commit per repository with the same message; a
/// failing hook in one stops there.
#[test]
fn commit_across_repositories() {
    let t = T::new();
    two_repos(&t);
    let (api, web) = (t.proj.join("api"), t.proj.join("web"));
    for r in [&api, &web] {
        no_sign(r);
    }
    let a = S::new(&t, A);
    a.start("startup");
    a.edit(&api.join("src/app.ts"), "A\n");
    a.edit(&web.join("src/app.ts"), "A\n");
    let r = a.tcas(&["commit", "-m", "same"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    for d in [&api, &web] {
        assert_eq!(head_files(d), ["src/app.ts"]);
        assert_eq!(git_text(d, &["log", "-1", "--format=%s"]).trim(), "same");
    }
    assert!(held_by(&t, A).is_empty(), "{:?}\n{}", leases(&t), r.out);

    // web/'s own pre-commit fails. api/ comes first, so it is committed;
    // web/ is not, and its lease stays.
    a.edit(&api.join("src/auth/session.ts"), "A2\n");
    a.edit(&web.join("src/auth/session.ts"), "A2\n");
    std::fs::write(web.join(".git/hooks/pre-commit"), "#!/bin/sh\necho 'lint failed' >&2\nexit 1\n").unwrap();
    std::process::Command::new("chmod").args(["+x"]).arg(web.join(".git/hooks/pre-commit")).status().unwrap();
    let r = a.tcas(&["commit", "-m", "second"]);
    assert_eq!(r.code, 1, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("failed"), "{}", r.out);
    assert_eq!(git_text(&api, &["log", "-1", "--format=%s"]).trim(), "second");
    assert_eq!(git_text(&web, &["log", "-1", "--format=%s"]).trim(), "same");
    assert_eq!(held_by(&t, A), [("web/src/auth/session.ts".into(), "edit".into())]);
}

// ---------------------------------------------------------------------------
// git hooks

/// Case 55: opening adds one include line; the repository's own hook runs
/// too and is left alone. Case 61: closing removes only that line.
#[test]
fn hooks_installed_beside_the_repositorys_own() {
    let t = T::new();
    let d = t.repo("app", &[("a.txt", "a\n")]);
    no_sign(&d);
    git(&d, &["config", "--add", "include.path", "/nonexistent/other.gitconfig"]);
    let own = d.join(".git/hooks/pre-commit");
    let mark = t.root.join("own-hook-ran");
    let script = format!("#!/bin/sh\ntouch {}\n", mark.display());
    std::fs::write(&own, &script).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&own).status().unwrap();
    let o = t.cmd(None, &["workspace", "open"]).current_dir(&d).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let inc = git_text(&d, &["config", "--local", "--get-all", "include.path"]);
    assert!(inc.contains("git-hooks.gitconfig") && inc.contains("other.gitconfig"), "{inc}");

    std::fs::write(d.join("a.txt"), "user\n").unwrap();
    let r = user_git(&d, &["-c", "core.hooksPath=.git/hooks", "commit", "-qam", "u"], &[]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(mark.exists(), "the repository's own hook ran");
    assert_eq!(read(&own), script);

    let o = t.cmd(None, &["workspace", "close"]).current_dir(&d).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let inc = git_text(&d, &["config", "--local", "--get-all", "include.path"]);
    assert!(!inc.contains("git-hooks.gitconfig") && inc.contains("other.gitconfig"), "{inc}");
    assert_eq!(read(&own), script);
}

/// Cases 56, 57: a script's commit is let through by the guard and refused
/// by pre-commit; the user's too, unless TCAS_ALLOW=1.
#[test]
fn pre_commit_refuses_others_files() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    b.prompt("Rework b");
    b.edit(&app.join("b.txt"), "B\n");
    std::fs::write(app.join("ship.sh"), "git add -A && git commit -q -m x\n").unwrap();
    let r = a.bash("sh ship.sh");
    assert!(!r.denied(), "{}", r.reason());
    assert_ne!(r.code(), 0);
    assert!(r.output().contains("commit refused") && r.output().contains("b.txt"), "{}", r.output());
    assert!(r.output().contains("leased by claude:bbbb…"), "{}", r.output());
    assert_eq!(git_text(&app, &["log", "--format=%s"]).lines().count(), 1);
    git(&app, &["reset", "-q"]);
    std::fs::remove_file(app.join("ship.sh")).unwrap();

    let r = user_git(&app, &["commit", "-qam", "x"], &[]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("commit refused") && r.err.contains("TCAS_ALLOW=1"), "{}", r.err);
    let r = user_git(&app, &["commit", "-qam", "x"], &[("TCAS_ALLOW", "1")]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(has_event(&t, "git.override", |_| true));
}

/// Case 58: `git commit -- a.txt` releases A's lease; a waiting session
/// gets it at once.
#[test]
fn plain_git_commit_releases() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    let (_, waiter) = b.bash_bg("tcas lease wait a.txt --timeout 20", "w");
    t.until("B waits", || t.daemon_waits() == 1);
    let r = a.bash("git commit -q -m x -- a.txt");
    assert_eq!(r.code(), 0, "{}", r.output());
    assert!(held_by(&t, A).is_empty(), "{:?}", leases(&t));
    let w = finish(waiter.unwrap());
    assert_eq!(w.code, 0, "{}\n{}", w.out, w.err);
    assert_eq!(held_by(&t, B), [("a.txt".into(), "wait".into())]);
}

/// Cases 59, 62: a lease left behind by an ended session: another session
/// may not commit the file, the user may, and is told whose it was.
#[test]
fn left_behind_files() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    a.prompt("Fix a");
    b.start("startup");
    a.edit(&app.join("a.txt"), "A\n");
    a.end("prompt_input_exit");
    assert_eq!(leases(&t)[0].3, "orphaned");

    let r = b.bash("git commit -q -m x -- a.txt");
    assert_ne!(r.code(), 0);
    assert!(r.output().contains("commit refused"), "{}", r.output());
    let r = user_git(&app, &["commit", "-q", "-m", "x", "--", "a.txt"], &[]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("left by sessions that have ended") && r.err.contains("claude:aaaa…"), "{}", r.err);
    assert!(leases(&t).is_empty(), "{:?}", leases(&t));
}

/// Case 60: stash and rebase, however started, are refused by git while
/// others hold leases; `git stash pop` is not affected.
#[test]
fn stash_and_rebase_refused_by_git() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    // A stash from before, and a branch that main has diverged from.
    std::fs::write(app.join("a.txt"), "stashed\n").unwrap();
    git(&app, &["stash", "-q"]);
    git(&app, &["branch", "side", "HEAD"]);
    git(&app, &["commit", "-q", "--allow-empty", "-m", "on main"]);
    git(&app, &["checkout", "-q", "side"]);
    git(&app, &["commit", "-q", "--allow-empty", "-m", "on side"]);
    git(&app, &["checkout", "-q", "main"]);

    b.edit(&app.join("b.txt"), "B\n");
    let s1 = t.root.join("s1.sh");
    let s2 = t.root.join("s2.sh");
    std::fs::write(&s1, "git stash -q\n").unwrap();
    // --autostash gets past git's own refusal of a dirty tree, to the hook.
    std::fs::write(&s2, "git rebase -q --autostash side\n").unwrap();
    let head = git_text(&app, &["rev-parse", "HEAD"]);
    let status = git_text(&app, &["status", "--porcelain"]);

    let r = a.sh(&format!("sh {}", s1.display()));
    assert_ne!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.err.contains("git stash refused"), "{}", r.err);
    let r = a.sh(&format!("sh {}", s2.display()));
    assert_ne!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.err.contains("git rebase refused"), "{}", r.err);
    assert_eq!(git_text(&app, &["status", "--porcelain"]), status);
    assert_eq!(git_text(&app, &["rev-parse", "HEAD"]), head);
    assert_eq!(read(&app.join("b.txt")), "B\n");
    assert_eq!(git_text(&app, &["stash", "list"]).lines().count(), 1);

    let r = a.sh("git stash pop -q");
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert_eq!(read(&app.join("a.txt")), "stashed\n");
}

/// Case 79: a session that opened the workspace cannot close it under
/// others' leases; later it can, and the hooks go.
#[test]
fn session_closes_its_workspace() {
    let t = T::new();
    let d = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    no_sign(&d);
    let (a, b) = (S::at(&t, A, &d), S::at(&t, B, &d));
    let r = a.tcas(&["workspace", "open"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    a.start("startup");
    b.start("startup");
    b.edit(&d.join("b.txt"), "B\n");
    assert_ne!(a.tcas(&["workspace", "close"]).code, 0);
    assert_eq!(b.tcas(&["commit", "-m", "b"]).code, 0);
    b.tcas(&["lease", "release", "--all"]);
    b.end("prompt_input_exit");
    let r = a.tcas(&["workspace", "close"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(!git_text(&d, &["config", "--local", "--list"]).contains("git-hooks.gitconfig"));
}

/// `post-merge` and `post-rewrite` hooks record `git.rewrite` events and
/// deliver rewrite notices to other sessions holding leases in the checkout.
#[test]
fn post_merge_and_post_rewrite_notify_holders() {
    let t = T::new();
    let app = app(&t);
    let b = S::at(&t, B, &app);
    b.start("startup");

    git(&app, &["checkout", "-q", "-b", "side"]);
    std::fs::write(app.join("c.txt"), "side\n").unwrap();
    git(&app, &["add", "c.txt"]);
    git(&app, &["commit", "-qm", "on side"]);
    git(&app, &["checkout", "-q", "main"]);
    std::fs::write(app.join("d.txt"), "main\n").unwrap();
    git(&app, &["add", "d.txt"]);
    git(&app, &["commit", "-qm", "on main"]);

    assert_eq!(b.tcas(&["lease", "acquire", "b.txt"]).code, 0);

    let r = user_git(&app, &["merge", "-q", "--no-edit", "side"], &[]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(
        has_event(&t, "git.rewrite", |e| e["payload"]["hook"] == "post-merge" && e["payload"]["what"] == "merge"),
        "{:?}",
        events(&t)
    );
    let ctx = b.post_other().context();
    assert!(ctx.contains("ran a git merge in") && ctx.contains("Re-read the files"), "{ctx}");

    // Switch to `side`, add a commit not in `main`, flush notices, then rebase onto `main`.
    git(&app, &["checkout", "-q", "side"]);
    std::fs::write(app.join("e.txt"), "side2\n").unwrap();
    git(&app, &["add", "e.txt"]);
    git(&app, &["commit", "-qm", "on side 2"]);
    b.post_other();
    let r = user_git(&app, &["rebase", "-q", "main"], &[("TCAS_ALLOW", "1")]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(
        has_event(&t, "git.rewrite", |e| e["payload"]["hook"] == "post-rewrite" && e["payload"]["what"] == "rebase"),
        "{:?}",
        events(&t)
    );
    let ctx = b.post_other().context();
    assert!(ctx.contains("ran a git rebase in") && ctx.contains("Re-read the files"), "{ctx}");
}

/// `tcas git-hooks status`, `remove`, and `install` inspect, remove, and
/// re-install `include.path` without touching other includes.
#[test]
fn git_hooks_status_remove_and_install() {
    let t = T::new();
    let app = app(&t);
    git(&app, &["config", "--add", "include.path", "/nonexistent/other.gitconfig"]);
    let a = S::at(&t, A, &app);
    a.start("startup");

    let run = |args: &[&str]| {
        let o = t.cmd(None, args).current_dir(&app).output().unwrap();
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    };

    let st = run(&["git-hooks", "status"]);
    assert_eq!(st.code, 0, "{}\n{}", st.out, st.err);
    assert!(st.out.contains(": installed"), "{}", st.out);

    // Sessions cannot remove or install git hooks.
    assert_eq!(a.tcas(&["git-hooks", "remove"]).code, 6);

    let rm = run(&["git-hooks", "remove"]);
    assert_eq!(rm.code, 0, "{}\n{}", rm.out, rm.err);
    assert!(rm.out.contains(": removed"), "{}", rm.out);
    let st = run(&["git-hooks", "status"]);
    assert!(st.out.contains(": not installed"), "{}", st.out);
    let inc = git_text(&app, &["config", "--local", "--get-all", "include.path"]);
    assert!(!inc.contains("git-hooks.gitconfig") && inc.contains("/nonexistent/other.gitconfig"), "{inc}");
    assert!(has_event(&t, "git_hooks.removed", |_| true));

    let ins = run(&["git-hooks", "install"]);
    assert_eq!(ins.code, 0, "{}\n{}", ins.out, ins.err);
    assert!(ins.out.contains(": installed"), "{}", ins.out);
    let again = run(&["git-hooks", "install"]);
    assert!(again.out.contains(": already installed"), "{}", again.out);
    let st = run(&["git-hooks", "status"]);
    assert!(st.out.contains(": installed"), "{}", st.out);
    let inc = git_text(&app, &["config", "--local", "--get-all", "include.path"]);
    assert!(inc.contains("git-hooks.gitconfig"), "{inc}");
}

// ---------------------------------------------------------------------------
// Submodules and worktrees

/// Case 46: a file in a submodule is leased under the submodule's path and
/// judged there; the outer repository's snapshot does not count it.
#[test]
fn submodules_are_their_own_checkouts() {
    let t = T::new();
    let src = t.root.join("proto-src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("x.proto"), "x\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "init"]);
    let api = t.repo("api", &[("src/app.ts", "a\n")]);
    no_sign(&api);
    let url = src.display().to_string();
    git(&api, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &url, "vendor/proto"]);
    git(&api, &["commit", "-qm", "submodule"]);
    t.ok(None, &["workspace", "open"]);

    let a = S::new(&t, A);
    a.start("startup");
    a.edit(&api.join("vendor/proto/x.proto"), "A\n");
    let r = a.bash("echo y > api/vendor/proto/y.proto");
    assert!(r.context().is_empty(), "{}", r.context());
    assert_eq!(
        held_by(&t, A),
        [("api/vendor/proto/x.proto".into(), "edit".into()), ("api/vendor/proto/y.proto".into(), "bash".into())]
    );
    let st = a.tcas(&["--json", "status"]).json();
    let x = st["leases"].as_array().unwrap().iter().find(|l| l["path"]["display"] == "api/vendor/proto/x.proto").cloned();
    assert_eq!(x.map(|l| l["dirty"].clone()), Some(Value::Bool(true)), "{st}");
}

/// Case 71: a worktree under an excluded directory is still found, and
/// Bash writes in it are reconciled.
#[test]
fn worktree_under_an_excluded_directory() {
    let t = T::new();
    let (_, wt) = with_inner_worktree(&t);
    let a = S::at(&t, A, &wt);
    a.start("startup");
    a.bash("echo n > n.txt");
    assert_eq!(held_by(&t, A), [(".claude/worktrees/x/n.txt".into(), "bash".into())]);
}

/// Case 75: a rebase is judged in the checkout it runs in.
#[test]
fn rebase_is_judged_per_worktree() {
    let t = T::new();
    let (d, wt) = with_inner_worktree(&t);
    let (a, b) = (S::at(&t, A, &d), S::at(&t, B, &wt));
    a.start("startup");
    b.start("startup");
    b.edit(&wt.join("b.txt"), "B\n");
    let r = a.bash("git rebase -q -f HEAD");
    assert!(!r.denied(), "{}", r.reason());
    assert_eq!(r.code(), 0, "{}", r.output());
    let r = a.bash("git -C .claude/worktrees/x rebase -q -f HEAD");
    assert!(r.denied());
    // Run past the guard, git's hook refuses it too.
    let r = a.sh("cd .claude/worktrees/x && git rebase -q --autostash -f HEAD");
    assert_ne!(r.code, 0);
    assert!(r.err.contains("git rebase refused"), "{}", r.err);
    assert_eq!(read(&wt.join("b.txt")), "B\n");
}

/// Case 76: a worktree removed under its leases: they go at the next
/// discovery, with `repo.gone`.
#[test]
fn removed_worktree_releases_its_leases() {
    let t = T::new();
    let (d, wt) = with_inner_worktree(&t);
    let (a, b) = (S::at(&t, A, &wt), S::at(&t, B, &d));
    a.start("startup");
    b.start("startup");
    a.edit(&wt.join("a.txt"), "A\n");
    assert_eq!(held_by(&t, A).len(), 1);
    git(&d, &["worktree", "remove", "--force", ".claude/worktrees/x"]);
    b.bash("true");
    assert!(held_by(&t, A).is_empty(), "{:?}", leases(&t));
    assert!(has_event(&t, "repo.gone", |e| e["payload"]["dir"] == ".claude/worktrees/x"), "{:?}", events(&t));
}

/// The main checkout and an outside worktree, each its own workspace.
fn split_worktree(t: &T) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    no_sign(&d);
    git(&d, &["worktree", "add", "-q", "-b", "x", "../app-x"]);
    let wt = t.proj.join("app-x");
    (d, wt)
}

fn open_at(t: &T, dir: &Path) {
    let o = t.cmd(None, &["workspace", "open"]).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

fn close_at(t: &T, dir: &Path, force: bool) {
    let mut args = vec!["workspace", "close"];
    if force {
        args.push("--force");
    }
    let o = t.cmd(None, &args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

fn has_hooks(d: &Path) -> bool {
    user_git(d, &["config", "--local", "--get-all", "include.path"], &[]).out.contains("git-hooks.gitconfig")
}

/// Case 73: closing one of two workspaces that share a repository keeps
/// the hooks; closing both removes them.
#[test]
fn shared_repository_keeps_hooks_until_both_close() {
    let t = T::new();
    let (d, wt) = split_worktree(&t);
    open_at(&t, &d);
    open_at(&t, &wt);
    let b = S::at(&t, B, &wt);
    b.start("startup");
    b.edit(&wt.join("b.txt"), "B\n");
    close_at(&t, &d, false);
    assert!(has_hooks(&wt));
    let r = user_git(&wt, &["commit", "-qam", "x"], &[]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("commit refused"), "{}", r.err);
    close_at(&t, &wt, true);
    assert!(!has_hooks(&d) && !has_hooks(&wt));
}

/// Case 74: only the worktree is a workspace; a commit in the main
/// checkout is let through without tcasd.
#[test]
fn hooks_outside_every_workspace_do_not_call_tcasd() {
    let t = T::new();
    let (d, wt) = split_worktree(&t);
    open_at(&t, &wt);
    assert!(has_hooks(&d), "the configuration is shared");
    t.ok(None, &["daemon", "stop"]);
    std::fs::write(d.join("a.txt"), "user\n").unwrap();
    let r = user_git(&d, &["commit", "-qam", "u"], &[]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.is_empty(), "{}", r.err);
    assert!(t.pid().is_none_or(|p| !alive(p)), "tcasd was started");
}

/// Case 83: a worktree made outside every workspace is hinted; adding it
/// works.
#[test]
fn worktree_made_outside_is_hinted() {
    let t = T::new();
    let app = app(&t);
    let a = S::at(&t, A, &app);
    a.start("startup");
    let r = a.bash("git worktree add -q -b x ../app-x");
    assert_eq!(r.code(), 0, "{}", r.output());
    assert!(r.context().contains("outside every workspace") && r.context().contains("tcas workspace add"), "{}", r.context());
    let r = a.tcas(&["workspace", "add", "../app-x"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(has_hooks(&t.proj.join("app-x")));
}

// ---------------------------------------------------------------------------
// Committed leases after the turn

/// Case 105: committed explicit leases are kept for the turn, and yield
/// once it ends; the holder hears first thing next turn.
#[test]
fn committed_leases_yield_after_the_turn() {
    let t = T::new();
    let app = app(&t);
    let (a, b, c) = (S::at(&t, A, &app), S::at(&t, B, &app), S::at(&t, C, &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }
    a.prompt("Rework auth");
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt", "src/auth/"]).code, 0);
    a.edit(&app.join("a.txt"), "A\n");
    a.edit(&app.join("src/auth/x.ts"), "A\n");
    let r = a.tcas(&["commit", "-m", "x"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    let (_, waiter) = b.bash_bg("tcas lease wait a.txt --timeout 20", "w");
    t.until("B waits", || t.daemon_waits() == 1);
    a.read(&app.join("b.txt"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(t.daemon_waits(), 1, "B got it during A's turn");

    a.stop();
    let w = finish(waiter.unwrap());
    assert_eq!(w.code, 0, "{}\n{}", w.out, w.err);
    assert_eq!(held_by(&t, B), [("a.txt".into(), "wait".into())]);
    assert!(!c.edit(&app.join("src/auth/y.ts"), "C\n").denied());

    let ctx = a.prompt("next").context();
    assert!(ctx.starts_with("tcas: while this session was idle"), "{ctx}");
    assert!(ctx.contains("a.txt") && ctx.contains("src/auth/"), "{ctx}");
}

/// Case 106: an uncommitted file is not given up when the turn ends; it is
/// marked stalled only after the stall time.
#[test]
fn uncommitted_leases_stay_after_the_turn() {
    let t = T::new();
    let app = app_with(&t, "[lease]\nstalled_after_secs = 2\n");
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt"]).code, 0);
    a.edit(&app.join("a.txt"), "A\n");
    let (_, waiter) = b.bash_bg("tcas lease wait a.txt --timeout 20", "w");
    t.until("B waits", || t.daemon_waits() == 1);
    a.stop();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(t.daemon_waits(), 1);
    let stalled = || {
        let st = b.tcas(&["--json", "status"]).json();
        st["leases"].as_array().unwrap().iter().find(|l| l["path"]["display"] == "a.txt").map(|l| l["stalled"] == true)
    };
    assert_eq!(stalled(), Some(false));
    std::thread::sleep(std::time::Duration::from_millis(2200));
    assert_eq!(stalled(), Some(true));
    assert_eq!(t.daemon_waits(), 1);
    let mut w = waiter.unwrap();
    let _ = w.kill();
    let _ = w.wait();
}
