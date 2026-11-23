//! End-to-end checks for milestone 1 (plan.md section 3): two sessions set
//! with `TCAS_SESSION` against a real tcasd, each test with its own home.

mod common;

use std::time::{Duration, Instant};

use common::*;

#[test]
fn open_discovers_both_repos() {
    let t = T::new();
    t.repo("api", &[("src/a.ts", "a\n"), (".gitignore", "vendor/\n")]);
    t.repo("web", &[("src/b.ts", "b\n")]);
    // A repository inside an excluded directory is not discovered.
    let vendored = t.proj.join("api/vendor/lib");
    std::fs::create_dir_all(&vendored).unwrap();
    git(&vendored, &["init", "-q", "-b", "main"]);

    let r = t.ok(None, &["--json", "workspace", "open"]).json();
    let mut dirs: Vec<String> =
        r["workspace"]["repos"].as_array().unwrap().iter().map(|x| x["dir"].as_str().unwrap().to_string()).collect();
    dirs.sort();
    assert_eq!(dirs, ["api", "web"]);
    assert!(r["workspace"]["repos"].as_array().unwrap().iter().all(|x| x["branch"] == "main"));

    let text = t.ok(None, &["workspace", "open"]).out;
    assert!(text.starts_with("already open"), "{text}");
    assert!(text.contains("api/") && text.contains("web/"), "{text}");

    let st = t.ok(None, &["--json", "status"]).json();
    assert_eq!(st["workspace"]["repos"].as_array().unwrap().len(), 2);
}

#[test]
fn acquire_deny_release_files_and_directories() {
    let t = T::new();
    two_repos(&t);
    let (a, b) = (Some("A"), Some("B"));

    let r = t.ok(a, &["lease", "acquire", "api/src/auth/session.ts", "--brief", "fix the login loop"]);
    assert!(r.out.starts_with("granted: api/src/auth/session.ts"), "{}", r.out);

    // A directory containing A's file is denied, all or nothing.
    let r = t.run(b, &["lease", "acquire", "web/src/app.ts", "api/src/auth/"]);
    assert_eq!(r.code, 3, "{}", r.out);
    assert!(r.out.starts_with("denied: nothing was acquired."), "{}", r.out);
    assert!(r.out.contains("api/src/auth/session.ts") && r.out.contains("leased by A"), "{}", r.out);
    assert!(r.out.contains("their brief: \"fix the login loop\""), "{}", r.out);
    assert!(r.out.contains("tcas lease wait web/src/app.ts api/src/auth/"), "{}", r.out);
    let mine = t.ok(b, &["--json", "lease", "list"]).json();
    assert_eq!(mine.as_array().unwrap().len(), 0, "denied request must not grant anything");

    // Paths in another repository, and git-excluded paths, are fine.
    let r = t.ok(b, &["lease", "acquire", "web/src/", "api/build/out.js"]);
    assert!(r.out.contains("granted: web/src/ (directory, 3 tracked files)"), "{}", r.out);
    assert!(r.out.contains("not managed, no lease needed:") && r.out.contains("excluded by git"), "{}", r.out);

    // A file inside B's directory lease is denied to A.
    let r = t.run(a, &["lease", "acquire", "web/src/auth/redirect.ts"]);
    assert_eq!(r.code, 3, "{}", r.out);
    assert!(r.out.contains("in web/src/, leased by B"), "{}", r.out);

    // Renewing your own lease is fine.
    let r = t.ok(a, &["lease", "acquire", "api/src/auth/session.ts"]);
    assert!(r.out.contains("already held, renewed"), "{}", r.out);

    // who, from the user.
    let r = t.ok(None, &["lease", "who", "api/src/auth/session.ts", "web/src/app.ts", "api/src/app.ts"]);
    assert!(r.out.contains("leased by A"), "{}", r.out);
    assert!(r.out.contains("web/src/ leased by B"), "{}", r.out);
    assert!(r.out.contains("api/src/app.ts") && r.out.contains("not leased"), "{}", r.out);

    // A changes its file: --all keeps it, naming it releases it with a warning.
    std::fs::write(t.proj.join("api/src/auth/session.ts"), "changed\n").unwrap();
    let r = t.ok(a, &["lease", "release", "--all"]);
    assert!(r.out.contains("still held, uncommitted changes: api/src/auth/session.ts"), "{}", r.out);
    let r = t.ok(a, &["lease", "release", "api/src/auth/session.ts"]);
    assert!(r.out.contains("warning: released api/src/auth/session.ts with uncommitted changes"), "{}", r.out);

    // Now B gets the directory.
    let r = t.ok(b, &["lease", "acquire", "api/src/auth/"]);
    assert!(r.out.contains("granted: api/src/auth/"), "{}", r.out);
    assert!(r.out.contains("has uncommitted changes that were not made by you"), "{}", r.out);

    // A directory overlapping B's from above is denied.
    let r = t.run(a, &["lease", "acquire", "api/src/"]);
    assert_eq!(r.code, 3, "{}", r.out);

    let r = t.ok(b, &["lease", "release", "api/src/auth/", "web/src/"]);
    assert!(r.out.contains("released: api/src/auth/, web/src/"), "{}", r.out);
    t.ok(a, &["lease", "acquire", "api/src/"]);

    let st = t.ok(None, &["--json", "status"]).json();
    let leases: Vec<(String, String)> = st["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["session"].as_str().unwrap().into(), l["path"]["display"].as_str().unwrap().into()))
        .collect();
    assert_eq!(leases, [("A".to_string(), "api/src/".to_string())]);
}

#[test]
fn who_may_do_what() {
    let t = T::new();
    two_repos(&t);
    t.ok(Some("A"), &["lease", "acquire", "api/src/app.ts"]);
    t.ok(Some("B"), &["lease", "list"]);

    let r = t.run(Some("B"), &["lease", "grant", "B", "api/src/app.ts"]);
    assert_eq!(r.code, 6, "{}", r.err);
    assert!(r.err.contains("only for the user"), "{}", r.err);
    assert_eq!(t.run(Some("B"), &["lease", "release", "--force", "api/src/app.ts"]).code, 6);
    assert_eq!(t.run(Some("B"), &["session", "end", "A"]).code, 6);
    assert_eq!(t.run(Some("B"), &["daemon", "stop"]).code, 6);

    // The user is not bound by leases and cannot take them.
    let r = t.run(None, &["lease", "acquire", "api/src/app.ts"]);
    assert_eq!(r.code, 1, "{}", r.err);

    // The user can move and revoke them.
    let r = t.ok(None, &["lease", "transfer", "api/src/app.ts", "--to", "B"]);
    assert!(r.out.contains("transferred to B"), "{}", r.out);
    let r = t.ok(None, &["lease", "release", "--force", "api/src/app.ts"]);
    assert!(r.out.contains("released: api/src/app.ts"), "{}", r.out);
    let r = t.ok(None, &["lease", "grant", "A", "web/src/auth/"]);
    assert!(r.out.contains("granted to A: web/src/auth/"), "{}", r.out);
}

#[test]
fn wait_is_granted_when_the_holder_releases() {
    let t = T::new();
    two_repos(&t);
    t.ok(Some("A"), &["lease", "acquire", "api/src/auth/session.ts"]);

    let waiter = t.spawn(Some("B"), &["lease", "wait", "api/src/auth/", "--timeout", "30", "--brief", "rename getUser"]);
    t.until("B's wait to register", || t.daemon_waits() == 1);
    let st = t.ok(None, &["--json", "status"]).json();
    let b = st["sessions"].as_array().unwrap().iter().find(|s| s["id"] == "B").unwrap().clone();
    assert_eq!(b["waiting_for"], serde_json::json!(["api/src/auth/"]));

    let released = Instant::now();
    t.ok(Some("A"), &["lease", "release", "api/src/auth/session.ts"]);
    let r = finish(waiter);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(r.out.starts_with("granted after"), "{}", r.out);
    assert!(released.elapsed() < Duration::from_secs(3), "grant took {:?}", released.elapsed());
    let mine = t.ok(Some("B"), &["--json", "lease", "list"]).json();
    assert_eq!(mine[0]["path"]["display"], "api/src/auth/");
    assert_eq!(mine[0]["source"], "wait");

    // A wait that runs out.
    let r = t.run(Some("A"), &["lease", "wait", "api/src/auth/redirect.ts", "--timeout", "1"]);
    assert_eq!(r.code, 4, "{}", r.out);
    assert!(r.out.starts_with("timed out after"), "{}", r.out);
    assert!(r.out.contains("still leased by B"), "{}", r.out);
}

#[test]
fn mutual_waits_the_later_is_refused() {
    let t = T::new();
    two_repos(&t);
    t.ok(Some("A"), &["lease", "acquire", "api/src/app.ts"]);
    t.ok(Some("B"), &["lease", "acquire", "web/src/app.ts"]);

    let first = t.spawn(Some("A"), &["lease", "wait", "web/src/app.ts", "--timeout", "30"]);
    t.until("A's wait to register", || t.daemon_waits() == 1);

    let r = t.run(Some("B"), &["lease", "wait", "api/src/app.ts", "--timeout", "30"]);
    assert_eq!(r.code, 5, "{}\n{}", r.out, r.err);
    assert!(r.out.starts_with("deadlock: not waiting."), "{}", r.out);
    assert!(r.out.contains("A holds api/src/app.ts and is waiting for web/src/app.ts, which you hold."), "{}", r.out);
    assert_eq!(t.daemon_waits(), 1, "the refused wait must not be registered");

    // The first wait still works.
    t.ok(Some("B"), &["lease", "release", "web/src/app.ts"]);
    let r = finish(first);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
}

#[test]
fn leases_survive_a_tcasd_restart() {
    let t = T::new();
    two_repos(&t);
    t.ok(Some("A"), &["lease", "acquire", "api/src/app.ts", "web/src/auth/", "--brief", "keep me"]);
    let waiter = t.spawn(Some("B"), &["lease", "wait", "api/src/app.ts", "--timeout", "30"]);
    t.until("B's wait to register", || t.daemon_waits() == 1);

    let old = t.pid().unwrap();
    kill(old);
    assert!(!alive(old));

    // The next command starts tcasd again; the leases are still there.
    let all = t.ok(None, &["--json", "lease", "list"]).json();
    let mut held: Vec<String> =
        all.as_array().unwrap().iter().map(|l| l["path"]["display"].as_str().unwrap().to_string()).collect();
    held.sort();
    assert_eq!(held, ["api/src/app.ts", "web/src/auth/"]);
    assert!(all.as_array().unwrap().iter().all(|l| l["session"] == "A" && l["brief"] == "keep me"));
    let new = t.pid().unwrap();
    assert_ne!(old, new);

    // B's wait reconnects and registers again.
    t.until("B's wait to register again", || t.daemon_waits() == 1);
    t.ok(Some("A"), &["lease", "release", "api/src/app.ts"]);
    let r = finish(waiter);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(r.out.starts_with("granted after"), "{}", r.out);

    let log = t.ok(None, &["--json", "log", "-n", "100"]).json();
    assert!(log.as_array().unwrap().iter().any(|e| e["type"] == "wait.resumed"), "{log}");

    // A clean stop followed by a start keeps them too.
    t.ok(None, &["daemon", "stop"]);
    let r = t.ok(None, &["daemon", "start"]);
    assert!(r.out.starts_with("tcasd started"), "{}", r.out);
    let all = t.ok(None, &["--json", "lease", "list"]).json();
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[test]
fn ended_session_keeps_dirty_files_as_orphans() {
    let t = T::new();
    two_repos(&t);
    t.ok(Some("A"), &["lease", "acquire", "api/src/app.ts", "api/src/auth/session.ts", "web/src/"]);
    std::fs::write(t.proj.join("api/src/app.ts"), "changed\n").unwrap();

    let r = t.ok(None, &["session", "end", "A"]);
    assert!(r.out.contains("2 leases released"), "{}", r.out);
    assert!(r.out.contains("orphaned (uncommitted changes remain): api/src/app.ts"), "{}", r.out);

    let r = t.run(Some("B"), &["lease", "acquire", "api/src/app.ts"]);
    assert_eq!(r.code, 3, "{}", r.out);
    assert!(r.out.contains("orphaned"), "{}", r.out);
    assert!(r.out.contains("Tell the user."), "{}", r.out);

    // Once the change is gone, the orphan goes too.
    git(&t.proj.join("api"), &["checkout", "--", "src/app.ts"]);
    t.ok(Some("B"), &["lease", "acquire", "api/src/app.ts"]);
}
