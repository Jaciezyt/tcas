//! Regression tests for the spec deviations found in the audit of
//! 2026-11-25. Each test fails on the code before the fix.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;

/// `app` holding `a.txt`, `b.txt` and a few files under `src/`, opened, with
/// a `.tcas.toml` committed.
fn app_with(t: &T, toml: &str) -> std::path::PathBuf {
    let app = one_repo(t);
    std::fs::write(app.join(".tcas.toml"), toml).unwrap();
    git(&app, &["add", ".tcas.toml"]);
    commit(&app, &[".tcas.toml"]);
    app
}

/// harness.md 5.1: `eval "$x"` and `sh -c "$CMD"` may be git and cannot be
/// checked; they are refused while another session holds leases in the
/// checkout. `git checkout <path> <path>` restores every path.
#[test]
fn computed_scripts_and_checkout_path_lists_are_guarded() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert!(!b.edit(&app.join("a.txt"), "B\n").denied());
    assert!(!a.edit(&app.join("b.txt"), "A\n").denied());

    for cmd in [r#"eval "$x""#, r#"bash -c "$CMD""#, r#"sh -c "cd . && git $SUB""#] {
        let r = a.pre_bash(cmd, false);
        assert!(r.denied(), "{cmd}: {}", r.v);
        assert!(r.reason().contains("explicitly"), "{cmd}: {}", r.reason());
    }
    assert!(!a.pre_bash(r#"bash -c "echo $HOME""#, false).denied());

    // `a.txt` is not a commit, so it is restored too: B's work.
    let r = a.pre_bash("git checkout a.txt b.txt", false);
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("a.txt"), "{}", r.reason());
    // A tree-ish first: only the paths after it count.
    assert!(!a.pre_bash("git checkout HEAD b.txt", false).denied());
}

/// harness.md 5.1: wrappers that change directory, `--git-dir`, and paths
/// reached through a symlink act on the checkout they name.
#[test]
fn guard_follows_env_chdir_git_dir_and_symlinks() {
    let t = T::new();
    two_repos(&t);
    let link = t.root.join("link");
    std::os::unix::fs::symlink(&t.proj, &link).unwrap();
    let (a, b) = (S::new(&t, "aaaa1111-2222"), S::new(&t, "bbbb3333-4444"));
    a.start("startup");
    b.start("startup");
    assert!(!b.edit(&t.proj.join("api/src/app.ts"), "B\n").denied());

    for cmd in [
        "env -C api git stash".to_string(),
        "sudo --chdir=api git stash".to_string(),
        r#"env -S "git -C api stash""#.to_string(),
        "git --git-dir=api/.git stash".to_string(),
        format!("cd {}/api && git stash", link.display()),
        format!("git -C {}/api reset --hard", link.display()),
    ] {
        let r = a.pre_bash(&cmd, false);
        assert!(r.denied(), "{cmd}: {}", r.v);
    }
    assert!(!a.pre_bash("env -C web git stash", false).denied());
    assert!(!a.pre_bash("git --git-dir=web/.git stash list", false).denied());
}

/// harness.md 5.1: `git worktree remove` is judged by its target, also when
/// the target is the worktree the command runs in.
#[test]
fn removing_the_current_worktree_is_judged_by_its_leases() {
    let t = T::new();
    let app = one_repo(&t);
    let wt = t.proj.join("app-x");
    git(&app, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let wt = std::fs::canonicalize(&wt).unwrap();
    let out = t.cmd(None, &["workspace", "add", wt.to_str().unwrap()]).current_dir(&app).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (a, b) = (S::at(&t, "aaaa1111-2222", &wt), S::at(&t, "bbbb3333-4444", &wt));
    a.start("startup");
    b.start("startup");
    assert!(!b.edit(&wt.join("a.txt"), "B\n").denied());

    let r = a.pre_bash("git worktree remove --force .", false);
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("worktree"), "{}", r.reason());
}

/// harness.md 5.3 and 6: what reconciling a background command finds at
/// `Stop` reaches the session at its next hook.
#[test]
fn background_overreach_found_at_stop_is_told_next_turn() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert!(!b.edit(&app.join("a.txt"), "B\n").denied());

    let (post, child) = a.bash_bg("sleep 1; echo A >> a.txt", "task1");
    assert!(!post.denied(), "{}", post.v);
    let mut child = child.unwrap();
    child.wait().unwrap();
    a.stop_with(&[]);
    let c = a.prompt("next").context();
    assert!(c.contains("your last command changed files that other sessions hold"), "{c}");
    assert!(c.contains("a.txt"), "{c}");
}

/// leases.md 6, "用到": using a file under one's own directory lease uses
/// the directory, also after the file has its own lease.
#[test]
fn editing_under_a_directory_lease_keeps_it() {
    let t = T::new();
    let app = app_with(&t, "[lease]\nexplicit_idle_secs = 2\n");
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    a.tcas(&["lease", "acquire", "src/auth/", "--brief", "auth work"]);
    for i in 0..4 {
        assert!(!a.edit(&app.join("src/auth/x.ts"), &format!("{i}\n")).denied());
        std::thread::sleep(Duration::from_millis(800));
    }
    let r = b.pre_edit(&app.join("src/auth/y.ts"));
    assert!(r.denied(), "the directory was in use: {}", r.v);
}

/// leases.md 6: acquiring a committed explicit lease again starts it over;
/// the earlier commit no longer lets it go when the turn ends.
#[test]
fn reacquiring_after_a_commit_keeps_the_lease_over_the_turn() {
    let t = T::new();
    let app = one_repo(&t);
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt"]).code, 0);
    assert!(!a.edit(&app.join("a.txt"), "A\n").denied());
    let c = a.bash("git commit -qm x -- a.txt");
    assert_eq!(c.code(), 0, "{}", c.output());
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt", "--brief", "second round"]).code, 0);
    a.stop();
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.denied(), "{}", r.v);
}

/// harness.md 5.6, post-commit: a stalled lease whose path someone else
/// committed is released.
#[test]
fn a_commit_releases_a_stalled_lease() {
    let t = T::new();
    let app = app_with(&t, "[lease]\nstalled_after_secs = 1\n");
    let (a, b) = (S::at(&t, "aaaa1111-2222", &app), S::at(&t, "bbbb3333-4444", &app));
    a.start("startup");
    b.start("startup");
    assert!(!a.edit(&app.join("a.txt"), "A\n").denied());
    std::thread::sleep(Duration::from_millis(1500));
    let r = b.pre_edit(&app.join("a.txt"));
    assert!(r.denied() && r.reason().contains("no activity"), "{}", r.v);

    let path = path_with_tcas();
    let home = t.home.to_string_lossy().into_owned();
    let env = [("TCAS_ALLOW", "1"), ("PATH", path.as_str()), ("TCAS_HOME", home.as_str())];
    let c = user_git(&app, &["commit", "-qm", "x", "--", "a.txt"], &env);
    assert_eq!(c.code, 0, "{}{}", c.out, c.err);
    t.until("the stalled lease to go", || !leases(&t).iter().any(|l| l.1 == "a.txt"));
    assert!(!b.edit(&app.join("a.txt"), "B\n").denied());
}

/// harness.md 5.6: `TCAS_ALLOW=0` is not an override.
#[test]
fn tcas_allow_zero_does_not_override() {
    let t = T::new();
    let app = one_repo(&t);
    let b = S::at(&t, "bbbb3333-4444", &app);
    b.start("startup");
    assert!(!b.edit(&app.join("a.txt"), "B\n").denied());
    let path = path_with_tcas();
    let home = t.home.to_string_lossy().into_owned();
    let env = [("TCAS_ALLOW", "0"), ("PATH", path.as_str()), ("TCAS_HOME", home.as_str())];
    let c = user_git(&app, &["commit", "-qm", "x", "--", "a.txt"], &env);
    assert_ne!(c.code, 0, "{}{}", c.out, c.err);
    assert!(c.err.contains("commit refused"), "{}", c.err);
}

/// daemon-and-cli.md 5: `daemon stop` is the user's even when tcasd is not
/// running, and `--json` always prints JSON.
#[test]
fn daemon_stop_rules_hold_when_it_is_not_running() {
    let t = T::new();
    let r = t.run(Some("A"), &["daemon", "stop"]);
    assert_eq!(r.code, 6, "{}{}", r.out, r.err);
    let r = t.ok(None, &["--json", "daemon", "stop"]);
    assert_eq!(r.json()["running"], false);
}

/// harness.md 5.5: only commands a session may run are approved; the
/// user-only `--force` of `lease release` is left to the harness.
#[test]
fn force_release_is_not_auto_approved() {
    let t = T::new();
    let app = one_repo(&t);
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    assert!(a.pre_bash("tcas lease release a.txt", false).allowed());
    let r = a.pre_bash("tcas lease release --force a.txt", false);
    assert!(!r.allowed(), "{}", r.v);
}

/// Built-in exemption check on case-insensitive volumes: `~/.tcas` in
/// another case is still tcas's own directory.
#[test]
fn tcas_home_in_another_case_is_protected() {
    let t = T::new();
    let app = one_repo(&t);
    if !Path::new(&t.home.to_string_lossy().to_uppercase()).exists() {
        return; // case-sensitive volume
    }
    let a = S::at(&t, "aaaa1111-2222", &app);
    a.start("startup");
    let upper = std::path::PathBuf::from(t.home.to_string_lossy().to_uppercase()).join("config.toml");
    assert!(a.pre_edit(&upper).denied());
    assert!(a.pre_bash(&format!("echo x >> {}", upper.display()), false).denied());
}
