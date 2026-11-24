//! End-to-end checks for milestone 4 (plan.md section 3 & 4):
//! read hints, freed/started/waiting/granted notices, SessionStart & compact
//! injection, late workspace open, strict mode, `.tcas.toml` `unmanaged`,
//! `exempt`, `roots`, 3rd-file acquire suggestion, and brief visibility/sanitization.

mod common;

use std::path::Path;

use common::*;

const A: &str = "aaaa1111-2222";
const B: &str = "bbbb3333-4444";
const C: &str = "cccc5555-6666";
const D: &str = "dddd7777-8888";

fn no_sign(dir: &Path) {
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
}

fn app(t: &T) -> std::path::PathBuf {
    let d = one_repo(t);
    no_sign(&d);
    d
}

fn has_event(t: &T, kind: &str, f: impl Fn(&serde_json::Value) -> bool) -> bool {
    events(t).iter().any(|e| e["type"] == kind && f(e))
}

/// Case 16: B reads a file held by A -> read hint once, not repeated.
/// Also checks directory-lease read hint.
#[test]
fn read_hint_shown_once_per_file() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");

    a.edit(&app.join("a.txt"), "A\n");
    let r1 = b.read(&app.join("a.txt")).context();
    assert!(r1.contains("a.txt is being changed by another session (claude:aaaa…)"), "{r1}");
    assert!(r1.contains("They have not written a brief."), "{r1}");
    assert!(r1.contains("work in progress"), "{r1}");
    assert!(b.read(&app.join("a.txt")).silent(), "second Read of a.txt must not repeat");

    a.tcas(&["lease", "acquire", "src/auth/", "--brief", "auth work"]);
    assert!(b.post_other().context().contains("has started work on src/auth/"));

    let r2 = b.read(&app.join("src/auth/x.ts")).context();
    assert!(r2.contains("src/auth/x.ts is in src/auth/, which is leased by another session (claude:aaaa…)"), "{r2}");
    assert!(r2.contains("Their brief: \"auth work\""), "{r2}");
    assert!(b.read(&app.join("src/auth/x.ts")).silent(), "second Read of x.ts must not repeat");
}

/// Case 17: denied edit -> holder commits -> `lease.freed` on next tool call -> edit succeeds;
/// and NO `lease.freed` when a waiter immediately takes the path.
#[test]
fn freed_notice_on_commit_and_suppressed_for_waiter() {
    let t = T::new();
    let app = app(&t);
    let (a, b, c, d) = (
        S::at(&t, A, &app),
        S::at(&t, B, &app),
        S::at(&t, C, &app),
        S::at(&t, D, &app),
    );
    for s in [&a, &b, &c, &d] {
        s.start("startup");
    }

    // Part A: B denied on a.txt, A commits -> B gets lease.freed with commit hash.
    a.edit(&app.join("a.txt"), "A\n");
    assert!(b.pre_edit(&app.join("a.txt")).denied());
    let cr = a.tcas(&["commit", "-m", "done a"]);
    assert_eq!(cr.code, 0, "{}\n{}", cr.out, cr.err);

    let ctx = b.post_other().context();
    assert!(ctx.contains("tcas: a.txt is now free (committed as "), "{ctx}");
    assert!(ctx.contains("Re-read it before editing."), "{ctx}");
    assert!(!b.edit(&app.join("a.txt"), "B\n").denied());

    // Part B: C denied on b.txt, D waits on b.txt, A commits b.txt -> D takes it, C gets NO lease.freed.
    a.edit(&app.join("b.txt"), "A\n");
    assert!(c.pre_edit(&app.join("b.txt")).denied());
    let waiter = d.spawn_tcas(&["lease", "wait", "b.txt", "--timeout", "20"]);
    t.until("D's wait registers", || t.daemon_waits() == 1);

    let cr2 = a.tcas(&["commit", "-m", "done b"]);
    assert_eq!(cr2.code, 0, "{}\n{}", cr2.out, cr2.err);
    let wr = finish(waiter);
    assert_eq!(wr.code, 0, "{}\n{}", wr.out, wr.err);

    let c_ctx = c.post_other().context();
    assert!(!c_ctx.contains("is now free"), "waiter took b.txt immediately; C must not get lease.freed: {c_ctx}");

    // Part C: A holds src/auth/ and dirty src/auth/x.ts, B denied on src/auth/x.ts,
    // A releases src/auth/ while src/auth/x.ts stays dirty -> B must NOT get lease.freed
    // yet, and only gets lease.freed once src/auth/x.ts is committed.
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/", "--brief", "auth work"]).code, 0);
    assert!(b.post_other().context().contains("has started work on src/auth/"));
    assert!(!a.edit(&app.join("src/auth/x.ts"), "dirty x\n").denied());
    assert!(b.pre_edit(&app.join("src/auth/x.ts")).denied());
    assert_eq!(a.tcas(&["lease", "release", "src/auth/"]).code, 0);
    let b_early = b.post_other().context();
    assert!(!b_early.contains("is now free"), "src/auth/x.ts is still dirty/held: {b_early}");
    let cr3 = a.tcas(&["commit", "-m", "done x"]);
    assert_eq!(cr3.code, 0, "{}\n{}", cr3.out, cr3.err);
    let b_after = b.post_other().context();
    assert!(b_after.contains("tcas: src/auth/x.ts is now free (committed as "), "{b_after}");
}

/// Case 18: user `lease grant B src/auth/y.ts` -> B gets notice on next hook, A denied.
#[test]
fn user_grant_notifies_recipient_and_blocks_other() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");

    let g = t.cmd(None, &["lease", "grant", &b.sid(), "src/auth/y.ts", "--brief", "handle y"])
        .current_dir(&app)
        .output()
        .unwrap();
    assert!(g.status.success(), "{}", String::from_utf8_lossy(&g.stderr));

    let ctx = b.post_other().context();
    assert!(ctx.contains("the user gave this session leases on: src/auth/y.ts"), "{ctx}");
    assert!(ctx.contains("The user's brief: \"handle y\""), "{ctx}");

    let r = a.pre_edit(&app.join("src/auth/y.ts"));
    assert!(r.denied());
    assert!(r.reason().contains("Their brief (written by the user): \"handle y\""), "{}", r.reason());
}

/// Case 19: `strict` mode denies unheld edit, asks to acquire first; succeeds after acquire.
#[test]
fn strict_mode_requires_acquire_first() {
    let t = T::new();
    let app = app(&t);
    std::fs::write(app.join(".tcas.toml"), "mode = \"strict\"\n").unwrap();
    let a = S::at(&t, A, &app);
    let start_ctx = a.start("startup").context();
    assert!(start_ctx.contains("mode: strict"), "{start_ctx}");

    let r = a.edit(&app.join("a.txt"), "A\n");
    assert!(r.denied(), "{}", r.v);
    assert!(r.reason().contains("strict mode"), "{}", r.reason());
    assert!(r.reason().contains("tcas lease acquire a.txt"), "{}", r.reason());
    assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "a\n");

    let acq = a.tcas(&["lease", "acquire", "a.txt", "--brief", "update a"]);
    assert_eq!(acq.code, 0, "{}\n{}", acq.out, acq.err);
    let r2 = a.edit(&app.join("a.txt"), "A\n");
    assert!(!r2.denied(), "{}", r2.v);
    assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "A\n");
}

/// Case 20 & repo-level `.tcas.toml`: `unmanaged` files can be edited/bash-written
/// concurrently without leases, `lease who` reports unmanaged pattern, and
/// existing leases are released if newly marked unmanaged.
#[test]
fn unmanaged_patterns_and_repo_level_toml() {
    let t = T::new();
    let app = t.repo(
        "app",
        &[
            ("a.txt", "a\n"),
            ("pnpm-lock.yaml", "lock\n"),
            ("src/btn.snap", "snap\n"),
            (".tcas.toml", "unmanaged = [\"pnpm-lock.yaml\", \"**/*.snap\"]\n"),
        ],
    );
    no_sign(&app);
    t.cmd(None, &["workspace", "open"]).current_dir(&app).output().unwrap();
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");

    assert!(!a.edit(&app.join("pnpm-lock.yaml"), "lock A\n").denied());
    assert!(!b.edit(&app.join("pnpm-lock.yaml"), "lock B\n").denied());
    let br = a.bash("echo snap2 > src/btn.snap");
    assert!(!br.denied(), "{}", br.reason());
    assert!(!b.edit(&app.join("src/btn.snap"), "snap3\n").denied());
    assert!(leases(&t).is_empty(), "{:?}", leases(&t));

    let st = a.tcas(&["--json", "status"]).json();
    assert!(st["unattributed"].as_array().unwrap().is_empty(), "{st}");

    let who = a.tcas(&["lease", "who", "pnpm-lock.yaml"]);
    assert_eq!(who.code, 0);
    assert!(who.out.contains("not managed: unmanaged pattern pnpm-lock.yaml"), "{}", who.out);

    // Multi-repo workspace with repo-level `.tcas.toml` in `web/` only.
    let t2 = T::new();
    let web = t2.repo("web", &[("gen.ts", "g\n"), ("sub/gen.ts", "sg\n"), ("cache/c.txt", "c\n")]);
    let api = t2.repo("api", &[("gen.ts", "g\n")]);
    no_sign(&web);
    no_sign(&api);
    std::fs::write(web.join(".tcas.toml"), "unmanaged = [\"/gen.ts\"]\nexempt = [\"cache/\"]\n").unwrap();
    t2.ok(None, &["workspace", "open"]);

    let (wa, wb) = (S::new(&t2, A), S::new(&t2, B));
    wa.start("startup");
    wb.start("startup");
    // `/gen.ts` in web/.tcas.toml unmanages web/gen.ts, not web/sub/gen.ts or api/gen.ts.
    assert!(!wa.edit(&web.join("gen.ts"), "1\n").denied());
    assert!(!wb.edit(&web.join("gen.ts"), "2\n").denied());
    assert!(!wa.edit(&web.join("cache/c.txt"), "1\n").denied());
    assert!(!wb.edit(&web.join("cache/c.txt"), "2\n").denied());

    assert!(!wa.edit(&web.join("sub/gen.ts"), "1\n").denied());
    assert!(wb.pre_edit(&web.join("sub/gen.ts")).denied());
    assert!(!wa.edit(&api.join("gen.ts"), "1\n").denied());
    assert!(wb.pre_edit(&api.join("gen.ts")).denied());

    // Newly adding `sub/gen.ts` to web/.tcas.toml releases the existing lease on edit.
    std::fs::write(web.join(".tcas.toml"), "unmanaged = [\"/gen.ts\", \"sub/gen.ts\"]\nexempt = [\"cache/\"]\n").unwrap();
    assert!(!wb.edit(&web.join("sub/gen.ts"), "2\n").denied());
    assert!(leases(&t2).iter().all(|l| l.1 != "web/sub/gen.ts"), "{:?}", leases(&t2));
}

/// Case 40: 3 implicit edits under `src/auth/` -> suggestion after 3rd, none after 4th.
#[test]
fn third_implicit_edit_suggests_acquire_once() {
    let t = T::new();
    let app = t.repo(
        "app",
        &[
            ("src/auth/a.ts", "a\n"),
            ("src/auth/b.ts", "b\n"),
            ("src/auth/c.ts", "c\n"),
            ("src/auth/d.ts", "d\n"),
        ],
    );
    no_sign(&app);
    t.cmd(None, &["workspace", "open"]).current_dir(&app).output().unwrap();

    let a = S::at(&t, A, &app);
    a.start("startup");
    assert!(a.edit(&app.join("src/auth/a.ts"), "a2\n").silent());
    assert!(a.edit(&app.join("src/auth/b.ts"), "b2\n").silent());
    let r3 = a.edit(&app.join("src/auth/c.ts"), "c2\n");
    let ctx3 = r3.context();
    assert!(ctx3.contains("you have now changed 3 files here without requesting leases first"), "{ctx3}");
    assert!(ctx3.contains("tcas lease acquire src/auth/ --brief"), "{ctx3}");

    let r4 = a.edit(&app.join("src/auth/d.ts"), "d2\n");
    assert!(r4.silent(), "4th edit must not repeat suggestion: {}", r4.v);
    assert!(has_event(&t, "lease.suggested", |e| e["payload"]["count"] == 3));
}

/// Cases 63, 64, 65, 68: brief visibility across denial/read/who, session brief
/// fallback on implicit edits, no brief message without leaking title, and
/// multiline/>200-char sanitization.
#[test]
fn brief_visibility_fallback_and_sanitization() {
    let t = T::new();
    let app = app(&t);
    let (a, b, c) = (S::at(&t, A, &app), S::at(&t, B, &app), S::at(&t, C, &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }

    // Case 65: C has title/prompt, never wrote brief, implicitly holds b.txt.
    c.prompt("Secret prompt C must never leak");
    c.edit(&app.join("b.txt"), "C\n");
    let d_b = b.pre_edit(&app.join("b.txt"));
    assert!(d_b.denied());
    assert!(d_b.reason().contains("They have not written a brief."), "{}", d_b.reason());
    assert!(!d_b.reason().contains("Secret prompt C"), "{}", d_b.reason());

    // Case 63 & 64: A has title/prompt and acquires src/auth/ with brief "Refactor auth flow".
    a.prompt("Secret prompt A must never leak");
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/", "--brief", "Refactor auth flow"]).code, 0);
    assert!(b.post_other().context().contains("has started work on src/auth/"));

    // 1. B edit denied
    let d1 = b.pre_edit(&app.join("src/auth/x.ts"));
    assert!(d1.denied());
    assert!(d1.reason().contains("Refactor auth flow"), "{}", d1.reason());
    assert!(!d1.reason().contains("Secret prompt A"), "{}", d1.reason());

    // 2. B reads src/auth/y.ts
    let r1 = b.read(&app.join("src/auth/y.ts")).context();
    assert!(r1.contains("Refactor auth flow"), "{r1}");
    assert!(!r1.contains("Secret prompt A"), "{r1}");

    // 3. B runs `tcas lease who`
    let who = b.tcas(&["lease", "who", "src/auth/x.ts"]);
    assert_eq!(who.code, 0);
    assert!(who.out.contains("Refactor auth flow"), "{}", who.out);
    assert!(!who.out.contains("Secret prompt A"), "{}", who.out);

    // Case 64: A implicitly edits src/util/z.ts -> B denied showing session brief "Refactor auth flow".
    a.edit(&app.join("src/util/z.ts"), "A\n");
    let d2 = b.pre_edit(&app.join("src/util/z.ts"));
    assert!(d2.denied());
    assert!(d2.reason().contains("Their brief: \"Refactor auth flow\""), "{}", d2.reason());
    assert!(!d2.reason().contains("Secret prompt A"), "{}", d2.reason());

    // Case 68: multiline + >200 chars -> first line truncated to 200 chars, shown in quotes.
    let long = format!("\n  {}\nsecond line ignored", "x".repeat(250));
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt", "--brief", &long]).code, 0);
    let d3 = b.pre_edit(&app.join("a.txt"));
    assert!(d3.denied());
    let expected = format!("\"{}\"", "x".repeat(200));
    assert!(d3.reason().contains(&expected), "{}", d3.reason());
    assert!(!d3.reason().contains(&"x".repeat(201)), "{}", d3.reason());
    assert!(!d3.reason().contains("second line ignored"), "{}", d3.reason());
}

/// Case 66: brief broadcast notifies active workspace members once per brief;
/// re-running with same brief does not re-notify; changing brief notifies again.
#[test]
fn brief_broadcast_once_per_brief() {
    let t = T::new();
    let app = app(&t);
    let (a, b, c) = (S::at(&t, A, &app), S::at(&t, B, &app), S::at(&t, C, &app));
    for s in [&a, &b, &c] {
        s.start("startup");
    }

    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/", "--brief", "X"]).code, 0);
    let b1 = b.post_other().context();
    let c1 = c.post_other().context();
    assert!(b1.contains("has started work on src/auth/") && b1.contains("\"X\""), "{b1}");
    assert!(c1.contains("has started work on src/auth/") && c1.contains("\"X\""), "{c1}");
    assert!(b.post_other().silent());
    assert!(c.post_other().silent());

    // Re-running with the same brief does not notify again.
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/", "--brief", "X"]).code, 0);
    assert!(b.post_other().silent());
    assert!(c.post_other().silent());

    // Changing brief notifies again.
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt", "--brief", "Z"]).code, 0);
    let b2 = b.post_other().context();
    let c2 = c.post_other().context();
    assert!(b2.contains("has started work on a.txt") && b2.contains("\"Z\""), "{b2}");
    assert!(c2.contains("has started work on a.txt") && c2.contains("\"Z\""), "{c2}");
}

/// Case 67: waiter notifies blocking holder once with waiter's brief.
#[test]
fn waiter_notifies_holder_once() {
    let t = T::new();
    let app = app(&t);
    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    a.start("startup");
    b.start("startup");

    a.edit(&app.join("a.txt"), "A\n");
    let mut w1 = b.spawn_tcas(&["lease", "wait", "a.txt", "--timeout", "20", "--brief", "Y"]);
    t.until("first wait registers", || t.daemon_waits() == 1);

    let ctx = a.post_other().context();
    assert!(ctx.contains("is waiting for a.txt, which you hold"), "{ctx}");
    assert!(ctx.contains("Their brief: \"Y\""), "{ctx}");
    assert!(ctx.contains("tcas lease release a.txt"), "{ctx}");
    assert!(a.post_other().silent(), "wait.waiting must be delivered once");

    // Cancel and re-wait on the same held lease: still no duplicate wait.waiting notice.
    let _ = w1.kill();
    let _ = w1.wait();
    t.until("first wait cleaned up", || t.daemon_waits() == 0);
    let mut w2 = b.spawn_tcas(&["lease", "wait", "a.txt", "--timeout", "20", "--brief", "Y2"]);
    t.until("second wait registers", || t.daemon_waits() == 1);
    assert!(a.post_other().silent(), "once per (waiter, holder, path)");
    let _ = w2.kill();
    let _ = w2.wait();
}

/// Case 77: no workspace yet; A and B already running; A runs `tcas workspace open`
/// and edits `a.txt`; B edits `a.txt` -> denied, git hooks installed, `workspace list`
/// shows opened by A, and B receives TCAS usage guidance.
#[test]
fn late_workspace_open_constrains_and_guides_existing_session() {
    let t = T::new();
    let app = t.repo("app", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
    no_sign(&app);

    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    assert!(a.start("startup").silent());
    assert!(b.start("startup").silent());
    assert!(t.pid().is_none(), "no workspace yet, tcasd must not start");

    let open = a.tcas(&["workspace", "open"]);
    assert_eq!(open.code, 0, "{}\n{}", open.out, open.err);

    let list = t.ok(None, &["--json", "workspace", "list"]).json();
    assert_eq!(list[0]["opened_by"], a.sid());
    let inc = git_text(&app, &["config", "--local", "--get-all", "include.path"]);
    assert!(inc.contains("git-hooks.gitconfig"), "{inc}");

    let ae = a.edit(&app.join("a.txt"), "A\n");
    assert!(!ae.denied());
    assert!(ae.context().contains("managed by TCAS"), "{}", ae.context());

    let be = b.edit(&app.join("a.txt"), "B\n");
    assert!(be.denied());
    assert!(be.reason().contains("leased by another session"), "{}", be.reason());
    assert!(be.reason().contains("managed by TCAS"), "denial includes TCAS guidance: {}", be.reason());

    let next = b.post_other().context();
    assert!(next.contains("managed by TCAS"), "next hook also delivers full intro: {next}");
}

/// Case 91: `app/.tcas.toml` has `roots = ["../app-backend"]` -> close & reopen
/// automatically adds `app-backend`, and SessionStart mentions extra roots & repos.
#[test]
fn toml_roots_auto_added_on_open() {
    let t = T::new();
    let app = t.repo("app", &[("a.txt", "a\n")]);
    let backend = t.repo("app-backend", &[("b.txt", "b\n")]);
    no_sign(&app);
    no_sign(&backend);
    std::fs::write(app.join(".tcas.toml"), "roots = [\"../app-backend\"]\n").unwrap();

    let o1 = t.cmd(None, &["workspace", "open"]).current_dir(&app).output().unwrap();
    assert!(o1.status.success(), "{}", String::from_utf8_lossy(&o1.stderr));
    let c1 = t.cmd(None, &["workspace", "close"]).current_dir(&app).output().unwrap();
    assert!(c1.status.success(), "{}", String::from_utf8_lossy(&c1.stderr));

    let o2 = t.cmd(None, &["--json", "workspace", "open"]).current_dir(&app).output().unwrap();
    assert!(o2.status.success(), "{}", String::from_utf8_lossy(&o2.stderr));
    let j: serde_json::Value = serde_json::from_slice(&o2.stdout).unwrap();
    let roots: Vec<&str> = j["workspace"]["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(roots, ["app", "app-backend"]);

    let a = S::at(&t, A, &app);
    let intro = a.start("startup").context();
    assert!(intro.contains("This workspace also covers: ../app-backend"), "{intro}");
    assert!(intro.contains("Repositories here:"), "{intro}");
}

/// Case 95 & SessionStart/compact: `.tcas.toml` `exempt = ["scratch/"]` edited
/// after open takes effect immediately in `tcas hook`, Bash reconcile, and
/// `lease acquire`; and `SessionStart` (`startup` / `compact`) injects & re-injects state.
#[test]
fn toml_exempt_and_session_start_compact_reinjection() {
    let t = T::new();
    let app = app(&t);
    std::fs::create_dir_all(app.join("scratch")).unwrap();
    std::fs::write(app.join("scratch/n.txt"), "n\n").unwrap();
    std::fs::write(app.join("scratch/m.txt"), "m\n").unwrap();
    git(&app, &["add", "scratch"]);
    commit(&app, &["scratch"]);

    // Edit .tcas.toml after workspace open to test live reload + workspaces.json sync.
    std::fs::write(app.join(".tcas.toml"), "exempt = [\"scratch/\"]\n").unwrap();

    let (a, b) = (S::at(&t, A, &app), S::at(&t, B, &app));
    let a_start = a.start("startup").context();
    assert!(a_start.contains("This directory is managed by TCAS (mode: auto)"), "{a_start}");
    a.prompt("Secret A title");
    a.edit(&app.join("a.txt"), "A\n");
    assert_eq!(a.tcas(&["lease", "acquire", "src/auth/", "--brief", "auth work"]).code, 0);

    // B's SessionStart sees A's brief and leases, never A's title.
    let b_start = b.start("startup").context();
    assert!(b_start.contains("Other sessions:"), "{b_start}");
    assert!(b_start.contains("\"auth work\""), "{b_start}");
    assert!(b_start.contains("src/auth/ (directory)"), "{b_start}");
    assert!(b_start.contains("a.txt"), "{b_start}");
    assert!(!b_start.contains("Secret A title"), "{b_start}");

    // A's compact re-injects intro and A's own leases.
    let a_compact = a.start("compact").context();
    assert!(a_compact.contains("This directory is managed by TCAS (mode: auto)"), "{a_compact}");
    assert!(a_compact.contains("Your leases here:"), "{a_compact}");
    assert!(a_compact.contains("a.txt") && a_compact.contains("src/auth/ (directory)"), "{a_compact}");

    // Case 95: scratch/ is exempt even though tracked.
    assert!(!b.edit(&app.join("scratch/n.txt"), "B\n").denied());
    let br = a.bash("echo x > scratch/m.txt");
    assert!(!br.denied(), "{}", br.reason());
    assert!(
        leases(&t).iter().all(|l| !l.1.starts_with("scratch/")),
        "exempt files must have no leases: {:?}",
        leases(&t)
    );
    let acq = a.tcas(&["lease", "acquire", "scratch/"]);
    assert_eq!(acq.code, 0, "{}\n{}", acq.out, acq.err);
    assert!(acq.out.contains("not managed") && acq.out.contains("exempt directory"), "{}", acq.out);

    // Confirm workspaces.json picked up `scratch/` as exempt.
    let wsj = std::fs::read_to_string(t.home.join("workspaces.json")).unwrap();
    assert!(wsj.contains("scratch/"), "{wsj}");
}
