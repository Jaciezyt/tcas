//! Milestone 5: `tcas stats` end-to-end tests (daemon-and-cli.md section 5,
//! plan.md section 5, walkthrough.md step 15).

mod common;

use std::time::Duration;

use common::{S, T, finish, git, user_git};

/// Reproduces the multi-repo scenario from walkthrough.md (api/ + web/,
/// sessions A, B, C, and the user) and checks both whole-workspace stats
/// and `--repo` per-checkout slices.
#[test]
fn walkthrough_scenario_and_repo_filter() {
    let t = T::new();
    t.repo(
        "api",
        &[
            ("src/auth/login.ts", "1\n"),
            ("src/auth/token.ts", "2\n"),
            ("src/auth/session.ts", "3\n"),
            ("src/auth/middleware.ts", "4\n"),
            ("src/auth/unused.ts", "5\n"),
            ("src/routes/export.ts", "export\n"),
        ],
    );
    t.repo(
        "web",
        &[
            ("src/export/button.tsx", "btn\n"),
            ("src/export/csv.ts", "csv\n"),
            ("src/export/table.tsx", "tbl\n"),
            ("src/api/client.ts", "client\n"),
            ("src/utils/date.ts", "date\n"),
        ],
    );
    t.ok(None, &["workspace", "open"]);

    let a = S::at(&t, "aaaa1111-0000", &t.proj);
    let b = S::at(&t, "bbbb2222-0000", &t.proj);
    let c = S::at(&t, "cccc3333-0000", &t.proj);
    a.start("startup");
    b.start("startup");
    c.start("startup");
    a.prompt("Secret prompt for A");
    b.prompt("Secret prompt for B");

    // Step 3: A acquires api/src/auth/ upfront with a brief and modifies 4 of its 5 files.
    let r = a.tcas(&["lease", "acquire", "api/src/auth/", "--brief", "Refactor auth tokens"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    for f in ["login.ts", "token.ts", "session.ts", "middleware.ts"] {
        assert!(!a.edit(&t.proj.join("api/src/auth").join(f), "changed\n").denied());
    }

    // C modifies two files in web/ implicitly, without upfront acquire or brief.
    assert!(!c.edit(&t.proj.join("web/src/api/client.ts"), "c-client\n").denied());
    assert!(!c.edit(&t.proj.join("web/src/utils/date.ts"), "c-date\n").denied());

    // Step 4: B modifies 3 files implicitly -> triggers `lease.suggested` -> B acquires upfront with brief.
    assert!(!b.edit(&t.proj.join("web/src/export/button.tsx"), "b1\n").denied());
    assert!(!b.edit(&t.proj.join("web/src/export/csv.ts"), "b2\n").denied());
    let third = b.edit(&t.proj.join("web/src/export/table.tsx"), "b3\n");
    assert!(!third.denied());
    assert!(third.context().contains("tcas lease acquire"), "{}", third.context());

    let r = b.tcas(&["lease", "acquire", "web/src/export/", "api/src/routes/", "--brief", "CSV export"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);

    // Command guard blocks A from `git -C web add -A` while B and C hold leases in web/.
    let blk = a.bash("git -C web add -A");
    assert!(blk.pre.denied(), "{}", blk.pre.v);

    // Bash overreach: A writes to C's leased `web/src/api/client.ts` via shell redirect.
    let ov = a.bash("printf 'overreach\\n' >> web/src/api/client.ts");
    assert!(!ov.pre.denied());
    assert!(ov.post.unwrap().context().contains("client.ts"));

    // Step 7: C exits leaving 2 orphaned leases in web/.
    c.end("exit");
    std::thread::sleep(Duration::from_millis(60));

    // Step 8: B tries to edit `web/src/api/client.ts` -> denied #1 -> B waits for it.
    assert!(b.edit(&t.proj.join("web/src/api/client.ts"), "b-client\n").denied());
    let w_b_client = b.spawn_tcas(&["lease", "wait", "web/src/api/client.ts", "--brief", "add export endpoint"]);
    t.until("B wait on client.ts", || t.daemon_waits() == 1);

    // Step 9: A tries to edit `api/src/routes/export.ts` (under B's dir lease) -> denied #2 -> A waits for it.
    assert!(a.edit(&t.proj.join("api/src/routes/export.ts"), "a-export\n").denied());
    let w_a_export = a.spawn_tcas(&["lease", "wait", "api/src/routes/export.ts", "--brief", "update auth in export"]);
    t.until("A wait on export.ts", || t.daemon_waits() == 2);

    // Step 11: User runs `git commit -am "wip"` in web/ (contains B's live dirty files) -> blocked by pre-commit.
    let ug = user_git(&t.proj.join("web"), &["commit", "-am", "wip"], &[]);
    assert_ne!(ug.code, 0, "expected pre-commit to block user commit: {}\n{}", ug.out, ug.err);

    // Deadlock wait: A also holds api/src/auth/login.ts and is waiting on B (export.ts);
    // B tries to edit `api/src/auth/login.ts` (denied #3) and waits -> immediate deadlock!
    assert!(b.edit(&t.proj.join("api/src/auth/login.ts"), "b-login\n").denied());
    let dl = b.tcas(&["lease", "wait", "api/src/auth/login.ts", "--brief", "touch login"]);
    assert_eq!(dl.code, 5, "{}\n{}", dl.out, dl.err);

    // Step 13: User commits C's two orphaned files -> orphans resolved, B's wait on client.ts granted!
    git(&t.proj.join("web"), &["add", "--", "src/api/client.ts", "src/utils/date.ts"]);
    git(&t.proj.join("web"), &["commit", "-m", "land C", "--", "src/api/client.ts", "src/utils/date.ts"]);
    let wb1 = finish(w_b_client);
    assert_eq!(wb1.code, 0, "{}\n{}", wb1.out, wb1.err);

    // B also waits on `web/src/utils/date.ts` (now free -> immediate wait grant) and edits both waited files.
    let wb2 = b.tcas(&["lease", "wait", "web/src/utils/date.ts", "--brief", "format export date"]);
    assert_eq!(wb2.code, 0, "{}\n{}", wb2.out, wb2.err);
    assert!(!b.edit(&t.proj.join("web/src/api/client.ts"), "b-client-2\n").denied());
    assert!(!b.edit(&t.proj.join("web/src/utils/date.ts"), "b-date-2\n").denied());
    assert_eq!(b.tcas(&["commit", "-m", "export web"]).code, 0);
    assert_eq!(b.tcas(&["lease", "release", "--all"]).code, 0);

    // Step 14 & 15: B released `api/src/routes/` -> A gets `export.ts`, edits and commits!
    let wa = finish(w_a_export);
    assert_eq!(wa.code, 0, "{}\n{}", wa.out, wa.err);
    assert!(!a.edit(&t.proj.join("api/src/routes/export.ts"), "a-export-2\n").denied());
    assert_eq!(a.tcas(&["commit", "-m", "auth + export"]).code, 0);
    assert_eq!(a.tcas(&["lease", "release", "--all"]).code, 0);

    let st = t.ok(None, &["--json", "stats"]).json();

    // Denials: 3 total, all 3 followed by wait attempts.
    assert_eq!(st["denials"]["total"], 3, "{st}");
    assert_eq!(st["denials"]["waited_after"], 3, "{st}");
    assert_eq!(st["denials"]["not_waited_after"], 0, "{st}");

    // Waits: 4 total (3 granted, 1 deadlock).
    assert_eq!(st["waits"]["total"], 4, "{st}");
    assert_eq!(st["waits"]["granted"], 3, "{st}");
    assert_eq!(st["waits"]["deadlock"], 1, "{st}");
    assert_eq!(st["waits"]["timeout"], 0, "{st}");

    // Modified files per session:
    // A: 5 total (4 explicit under api/src/auth/, 0 implicit, 1 waited export.ts)
    // B: 5 total (0 explicit, 3 implicit, 2 waited client.ts + date.ts)
    // C: 2 total (0 explicit, 2 implicit, 0 waited)
    let by_sess = st["files_by_session"].as_array().unwrap();
    let fa = by_sess.iter().find(|x| x["session"] == a.sid()).unwrap();
    assert_eq!((fa["total"].as_u64(), fa["explicit"].as_u64(), fa["implicit"].as_u64(), fa["waited"].as_u64()), (Some(5), Some(4), Some(0), Some(1)), "{fa}");
    assert_eq!(fa["title"], "Secret prompt for A");
    let fb = by_sess.iter().find(|x| x["session"] == b.sid()).unwrap();
    assert_eq!((fb["total"].as_u64(), fb["explicit"].as_u64(), fb["implicit"].as_u64(), fb["waited"].as_u64()), (Some(5), Some(0), Some(3), Some(2)), "{fb}");
    let fc = by_sess.iter().find(|x| x["session"] == c.sid()).unwrap();
    assert_eq!((fc["total"].as_u64(), fc["explicit"].as_u64(), fc["implicit"].as_u64(), fc["waited"].as_u64()), (Some(2), Some(0), Some(2), Some(0)), "{fc}");

    // Directory leases: 3 total; api/src/auth/ has 5 tracked files and 4 modified.
    let dls = st["directory_leases"].as_array().unwrap();
    assert_eq!(dls.len(), 3, "{dls:?}");
    let d_auth = dls.iter().find(|d| d["path"] == "api/src/auth/").unwrap();
    assert_eq!(d_auth["tracked_files"], 5, "{d_auth}");
    assert_eq!(d_auth["modified_files"], 4, "{d_auth}");

    // Acquire suggestions: 1 suggested (B), 1 acquired afterwards.
    assert_eq!(st["suggestions"]["suggested"], 1, "{st}");
    assert_eq!(st["suggestions"]["acquired_after"], 1, "{st}");

    // Upfront briefs: 2 upfront `lease acquire` invocations (A's 1 dir + B's 2 dirs in one call), both with brief!
    assert_eq!(st["upfront_acquires"]["total"], 2, "{st}");
    assert_eq!(st["upfront_acquires"]["with_brief"], 2, "{st}");
    assert_eq!(st["upfront_acquires"]["without_brief"], 0, "{st}");

    // Bash overreach, command guard, git hooks, orphaned leases.
    assert_eq!(st["bash"]["overreach"], 1, "{st}");
    assert_eq!(st["guard_blocked"]["total"], 1, "{st}");
    assert_eq!(st["guard_blocked"]["by_rule"][0]["rule"], "add_all", "{st}");
    assert_eq!(st["git_blocked"]["total"], 1, "{st}");
    assert_eq!(st["git_blocked"]["by_user"], 1, "{st}");
    assert_eq!(st["git_blocked"]["missed_by_guard"], 0, "{st}");
    assert_eq!(st["orphaned"]["total"], 2, "{st}");
    assert_eq!(st["orphaned"]["current"], 0, "{st}");
    assert!(st["orphaned"]["durations"]["max_ms"].as_i64().unwrap() >= 50, "{st}");

    // Human-readable output runs cleanly and hides nothing for the user, while
    // an agent session never sees other sessions' prompt titles.
    let human_user = t.ok(None, &["stats"]).out;
    assert!(human_user.contains("contention & waits:") && human_user.contains("Secret prompt for A"), "{human_user}");

    let agent_json = a.tcas(&["--json", "stats"]).json();
    assert!(agent_json["files_by_session"].as_array().unwrap().iter().all(|x| x.get("title").is_none()), "{agent_json}");
    let agent_human = a.tcas(&["stats"]).out;
    assert!(!agent_human.contains("Secret prompt"), "{agent_human}");

    // `tcas stats` is auto-approved by the Claude Code PreToolUse Bash hook.
    assert!(a.pre_bash("tcas stats", false).allowed());

    // `--repo api` vs `--repo web` slice accurately.
    let st_api = t.ok(None, &["--json", "stats", "--repo", "api"]).json();
    assert_eq!(st_api["repo"], "api/");
    assert_eq!(st_api["denials"]["total"], 2, "{st_api}"); // export.ts + login.ts
    assert_eq!(st_api["files"]["total"], 5, "{st_api}"); // 4 auth + 1 export.ts
    assert_eq!(st_api["bash"]["overreach"], 0, "{st_api}");
    assert_eq!(st_api["guard_blocked"]["total"], 0, "{st_api}");
    assert_eq!(st_api["git_blocked"]["total"], 0, "{st_api}");
    assert_eq!(st_api["orphaned"]["total"], 0, "{st_api}");

    let st_web = t.ok(None, &["--json", "stats", "--repo", "web"]).json();
    assert_eq!(st_web["repo"], "web/");
    assert_eq!(st_web["denials"]["total"], 1, "{st_web}"); // client.ts
    assert_eq!(st_web["files"]["total"], 7, "{st_web}"); // B's 5 + C's 2
    assert_eq!(st_web["bash"]["overreach"], 1, "{st_web}");
    assert_eq!(st_web["guard_blocked"]["total"], 1, "{st_web}");
    assert_eq!(st_web["git_blocked"]["total"], 1, "{st_web}");
    assert_eq!(st_web["orphaned"]["total"], 2, "{st_web}");

    let bad_repo = t.run(None, &["stats", "--repo", "nonexistent"]);
    assert_eq!(bad_repo.code, 1);
}

/// Tests yields (30m explicit idle, 10m waited grace, post-turn committed_idle
/// next-turn vs later-turn re-request), stalled lease lifecycle, guard dynamic
/// blocks, git hook missed_by_guard, TCAS_ALLOW=1 overrides, and time bounds.
#[test]
fn yields_stalled_guard_hooks_and_windows() {
    let t = T::new();
    let app = t.repo(
        "app",
        &[
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("c.txt", "c\n"),
            ("d.txt", "d\n"),
            ("e.txt", "e\n"),
            ("stall.txt", "s\n"),
            (".tcas.toml", "[lease]\nexplicit_idle_secs = 1\nwaited_grace_secs = 1\nstalled_after_secs = 2\n"),
        ],
    );
    t.ok_at(&app, None, &["workspace", "open"]);

    let a = S::at(&t, "aaaa1111-0000", &app);
    let b = S::at(&t, "bbbb2222-0000", &app);
    a.start("startup");
    b.start("startup");

    // 1. Explicit file acquire without brief; edit only `a.txt`, leave `b.txt` untouched.
    assert_eq!(a.tcas(&["lease", "acquire", "a.txt", "b.txt"]).code, 0);
    assert!(!a.edit(&app.join("a.txt"), "a1\n").denied());

    // B denied on `a.txt` twice without waiting (`not_waited_after == 2`, hotspot count 2).
    assert!(b.edit(&app.join("a.txt"), "b1\n").denied());
    assert!(b.edit(&app.join("a.txt"), "b2\n").denied());

    // Wait > 1s so `b.txt` (clean explicit lease) expires (`explicit_idle`).
    std::thread::sleep(Duration::from_millis(1200));
    // B takes `b.txt` -> `explicit_idle` yield #1; A later tries to edit `b.txt` -> re-requested!
    assert!(!b.edit(&app.join("b.txt"), "b-took\n").denied());
    assert!(a.edit(&app.join("b.txt"), "a-wants-back\n").denied());
    assert_eq!(b.tcas(&["commit", "-m", "b"]).code, 0);

    // 2. Waited grace expiry: B waits on `a.txt`; A commits and releases `a.txt`;
    // B gets `a.txt` via wait, does NOT edit it, > 1s passes, C takes it (B never re-requests).
    let w = b.spawn_tcas(&["lease", "wait", "a.txt", "--brief", "wait for a"]);
    t.until("B wait", || t.daemon_waits() == 1);
    assert_eq!(a.tcas(&["commit", "-m", "a"]).code, 0);
    assert_eq!(a.tcas(&["lease", "release", "a.txt"]).code, 0);
    assert_eq!(finish(w).code, 0);
    std::thread::sleep(Duration::from_millis(1200));
    let c = S::at(&t, "cccc3333-0000", &app);
    c.start("startup");
    assert!(!c.edit(&app.join("a.txt"), "c-took\n").denied());
    assert_eq!(c.tcas(&["commit", "-m", "c"]).code, 0);

    // 3. Post-turn `committed_idle` yields:
    // Case 3a: A acquires `c.txt` --brief, edits + commits, stops turn (turn 0 -> 1);
    // B takes `c.txt` (`committed_idle` #1); A immediately tries `c.txt` in its next turn -> reacquired_next_turn!
    assert_eq!(a.tcas(&["lease", "acquire", "c.txt", "--brief", "work on c"]).code, 0);
    assert!(!a.edit(&app.join("c.txt"), "c1\n").denied());
    assert_eq!(a.tcas(&["commit", "-m", "c1"]).code, 0);
    a.stop();
    assert!(!b.edit(&app.join("c.txt"), "b-c\n").denied());
    assert!(a.edit(&app.join("c.txt"), "a-next-turn\n").denied()); // in A's next turn!
    a.stop();
    assert_eq!(b.tcas(&["commit", "-m", "b-c"]).code, 0);

    // Case 3b: A acquires `d.txt` --brief, edits + commits, stops turn;
    // B takes `d.txt` (`committed_idle` #2); A runs an unrelated turn and stops (`turn` advances!),
    // and ONLY requests `d.txt` in the turn AFTER next -> NOT counted as `reacquired_next_turn`!
    assert_eq!(a.tcas(&["lease", "acquire", "d.txt", "--brief", "work on d"]).code, 0);
    assert!(!a.edit(&app.join("d.txt"), "d1\n").denied());
    assert_eq!(a.tcas(&["commit", "-m", "d1"]).code, 0);
    a.stop();
    assert!(!b.edit(&app.join("d.txt"), "b-d\n").denied());
    // A's next turn touches `e.txt`, not `d.txt`, then ends!
    assert!(!a.edit(&app.join("e.txt"), "e1\n").denied());
    assert_eq!(a.tcas(&["commit", "-m", "e1"]).code, 0);
    a.stop();
    // Two turns later A edits `d.txt`:
    assert!(a.edit(&app.join("d.txt"), "a-two-turns-later\n").denied());
    assert_eq!(b.tcas(&["commit", "-m", "b-d"]).code, 0);

    // 4. Stalled lease: A edits `stall.txt` (dirty), goes idle > 2s.
    assert!(!a.edit(&app.join("stall.txt"), "dirty-stall\n").denied());
    a.stop();
    std::thread::sleep(Duration::from_millis(2200));

    // Check while currently stalled!
    let mid = t.ok_at(&app, None, &["--json", "stats"]).json();
    assert_eq!(mid["stalled"]["total"], 1, "{mid}");
    assert_eq!(mid["stalled"]["current"], 1, "{mid}");

    // Guard dynamic refusal (`git $SUB`) while A holds dirty `stall.txt`:
    assert!(b.bash("SUB=status; git $SUB").pre.denied());

    // Git hook missed by guard: B runs a shell script committing `stall.txt` (guarded only by pre-commit)!
    std::fs::write(app.join("sneak.sh"), "#!/bin/sh\ngit add stall.txt && git commit -m sneak\n").unwrap();
    let sneak = b.bash("sh sneak.sh");
    assert!(!sneak.pre.denied());
    assert_ne!(sneak.run.unwrap().code, 0);

    // User override with TCAS_ALLOW=1 on `stall.txt`:
    git(&app, &["add", "--", "stall.txt"]);
    let blocked_user = user_git(&app, &["commit", "-m", "user blocked", "--", "stall.txt"], &[]);
    assert_ne!(blocked_user.code, 0);
    let allowed_user = user_git(&app, &["commit", "-m", "user override", "--", "stall.txt"], &[("TCAS_ALLOW", "1")]);
    assert_eq!(allowed_user.code, 0, "{}\n{}", allowed_user.out, allowed_user.err);

    // Wait timeout: B holds dirty `e.txt`, C waits with `--timeout 1`.
    assert!(!b.edit(&app.join("e.txt"), "e-dirty\n").denied());
    let w_to = c.tcas(&["lease", "wait", "e.txt", "--timeout", "1"]);
    assert_eq!(w_to.code, 4, "{}\n{}", w_to.out, w_to.err);
    assert_eq!(b.tcas(&["commit", "-m", "e-dirty"]).code, 0);

    let st = t.ok_at(&app, None, &["--json", "stats"]).json();

    // Verify stalled episode completed once `stall.txt` was committed.
    assert_eq!(st["stalled"]["total"], 1, "{st}");
    assert_eq!(st["stalled"]["current"], 0, "{st}");

    // Verify expired yields: 1 explicit_idle (reacquired=1) + 1 waited_grace (reacquired=0).
    assert_eq!(st["yields"]["expired"]["total"], 2, "{st}");
    assert_eq!(st["yields"]["expired"]["explicit_idle"], 1, "{st}");
    assert_eq!(st["yields"]["expired"]["explicit_idle_reacquired"], 1, "{st}");
    assert_eq!(st["yields"]["expired"]["waited_grace"], 1, "{st}");
    assert_eq!(st["yields"]["expired"]["waited_grace_reacquired"], 0, "{st}");
    assert_eq!(st["yields"]["expired"]["reacquired"], 1, "{st}");

    // Verify post-turn committed_idle yields: 2 total, only 1 re-requested in next turn!
    assert_eq!(st["yields"]["committed_idle"]["total"], 2, "{st}");
    assert_eq!(st["yields"]["committed_idle"]["reacquired_next_turn"], 1, "{st}");

    // Verify upfront briefs: 3 upfront acquires (1 without brief, 2 with brief).
    assert_eq!(st["upfront_acquires"]["total"], 3, "{st}");
    assert_eq!(st["upfront_acquires"]["with_brief"], 2, "{st}");
    assert_eq!(st["upfront_acquires"]["without_brief"], 1, "{st}");

    // Verify untouched `b.txt` was NOT counted as modified by A (A modified a, c, d, e, stall = 5 files).
    let fa = st["files_by_session"].as_array().unwrap().iter().find(|x| x["session"] == a.sid()).unwrap();
    assert_eq!(fa["total"], 5, "{fa}");
    assert_eq!(fa["explicit"], 3, "{fa}"); // a.txt, c.txt, d.txt (b.txt was never edited by A!)
    assert_eq!(fa["implicit"], 2, "{fa}"); // e.txt, stall.txt

    // Verify guard dynamic, git hook missed_by_guard + by_user, and TCAS_ALLOW=1 override.
    assert_eq!(st["guard_blocked"]["total"], 1, "{st}");
    assert_eq!(st["guard_blocked"]["dynamic"], 1, "{st}");
    assert_eq!(st["git_blocked"]["total"], 2, "{st}");
    assert_eq!(st["git_blocked"]["missed_by_guard"], 1, "{st}");
    assert_eq!(st["git_blocked"]["by_user"], 1, "{st}");
    assert_eq!(st["git_overrides"]["total"], 1, "{st}");
    assert_eq!(st["waits"]["timeout"], 1, "{st}");

    // Time window filtering (`--since`, `--until`, `--workspace`) and error handling.
    let recent = t.ok(None, &["--json", "stats", "--workspace", "app", "--since", "1h"]).json();
    assert_eq!(recent["events"], st["events"]);
    let future_empty = t.ok_at(&app, None, &["--json", "stats", "--until", "2020-01-01"]).json();
    assert_eq!(future_empty["events"], 0, "{future_empty}");
    assert_eq!(t.run(None, &["stats", "--workspace", "app", "--since", "bad-time"]).code, 1);
}

/// Multi-root `--repo` filtering, concurrent Bash `unattributed`, active and
/// cancelled waits, Bash edits under an upfront file lease, and upgrading
/// implicit file leases after `lease.suggested`.
#[test]
fn multi_root_unattributed_active_wait_and_upgrade() {
    let t = T::new();
    let front = t.repo("app", &[("f1.txt", "1\n"), ("f2.txt", "2\n"), ("f3.txt", "3\n"), ("gen.txt", "g\n"), ("keep.txt", "k\n")]);
    let back = t.repo("app-backend", &[("api.rs", "fn a() {}\n")]);
    t.ok_at(&front, None, &["workspace", "open"]);
    t.ok_at(&front, None, &["workspace", "add", "../app-backend"]);

    let a = S::at(&t, "aaaa1111-0000", &front);
    let b = S::at(&t, "bbbb2222-0000", &front);
    a.start("startup");
    b.start("startup");

    // 1. Implicit -> explicit upgrade on the exact files after `lease.suggested`:
    // files were edited BEFORE acquiring explicitly, so they remain `implicit`,
    // while `suggestions.acquired_after` and `upfront_acquires` both count the acquire.
    assert!(!a.edit(&front.join("f1.txt"), "1x\n").denied());
    assert!(!a.edit(&front.join("f2.txt"), "2x\n").denied());
    let r3 = a.edit(&front.join("f3.txt"), "3x\n");
    assert!(r3.context().contains("tcas lease acquire"), "{}", r3.context());
    assert_eq!(a.tcas(&["lease", "acquire", "f1.txt", "f2.txt", "f3.txt", "--brief", "upgrade exact files"]).code, 0);

    // 2. Upfront file lease modified via Bash (`gen.txt`), while `keep.txt` stays untouched.
    assert_eq!(b.tcas(&["lease", "acquire", "gen.txt", "keep.txt", "../app-backend/api.rs", "--brief", "gen + backend"]).code, 0);
    assert!(!b.bash("printf 'generated\\n' > gen.txt").pre.denied());
    assert!(!b.edit(&back.join("api.rs"), "fn a() { 1 }\n").denied());

    // 3. Concurrent Bash windows -> unattributed file in `app/`.
    let id_a = "call-bg-a";
    let in_a = serde_json::json!({ "command": "sleep 1", "description": "bg" });
    a.hook(serde_json::json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": in_a, "tool_use_id": id_a }));
    b.bash("printf 'mystery\\n' > mystery.txt");
    a.hook(serde_json::json!({
        "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": in_a, "tool_use_id": id_a,
        "tool_response": { "stdout": "", "stderr": "", "interrupted": false }
    }));

    // 4. Active wait in `app-backend`, then client disconnects (`wait.cancelled`).
    let mut waiter = a.spawn_tcas(&["lease", "wait", "../app-backend/api.rs", "--brief", "need backend"]);
    t.until("active wait", || t.daemon_waits() == 1);

    let live_back = t.ok_at(&front, None, &["--json", "stats", "--repo", "app-backend"]).json();
    assert_eq!(live_back["waits"]["active"], 1, "{live_back}");
    assert_eq!(live_back["waits"]["total"], 1, "{live_back}");
    assert_eq!(live_back["files"]["explicit"], 1, "{live_back}");
    assert_eq!(live_back["bash"]["unattributed"], 0, "{live_back}");

    let _ = waiter.kill();
    let _ = waiter.wait();
    t.until("wait cancelled", || t.daemon_waits() == 0);

    let st = t.ok_at(&front, None, &["--json", "stats"]).json();
    assert_eq!(st["suggestions"]["suggested"], 1, "{st}");
    assert_eq!(st["suggestions"]["acquired_after"], 1, "{st}");
    assert_eq!(st["upfront_acquires"]["total"], 2, "{st}");
    assert_eq!(st["upfront_acquires"]["with_brief"], 2, "{st}");
    // Both overlapping Bash windows reconcile and report the unattributed file.
    assert_eq!(st["bash"]["unattributed"], 2, "{st}");
    assert_eq!(st["waits"]["cancelled"], 1, "{st}");
    assert_eq!(st["waits"]["active"], 0, "{st}");

    let by_sess = st["files_by_session"].as_array().unwrap();
    let fa = by_sess.iter().find(|x| x["session"] == a.sid()).unwrap();
    assert_eq!((fa["total"].as_u64(), fa["explicit"].as_u64(), fa["implicit"].as_u64()), (Some(3), Some(0), Some(3)), "{fa}");
    let fb = by_sess.iter().find(|x| x["session"] == b.sid()).unwrap();
    assert_eq!((fb["total"].as_u64(), fb["explicit"].as_u64(), fb["implicit"].as_u64()), (Some(2), Some(2), Some(0)), "{fb}");

    let st_front = t.ok_at(&front, None, &["--json", "stats", "--repo", "app"]).json();
    assert_eq!(st_front["bash"]["unattributed"], 2, "{st_front}");
    assert_eq!(st_front["files"]["total"], 4, "{st_front}"); // f1, f2, f3, gen.txt (keep.txt untouched)
    assert_eq!(st_front["waits"]["cancelled"], 0, "{st_front}");
}

trait TAt {
    fn ok_at(&self, dir: &std::path::Path, session: Option<&str>, args: &[&str]) -> common::Run;
}

impl TAt for T {
    fn ok_at(&self, dir: &std::path::Path, session: Option<&str>, args: &[&str]) -> common::Run {
        let mut c = self.cmd(session, args);
        c.current_dir(dir);
        let o = c.stdin(std::process::Stdio::null()).output().unwrap();
        let r = common::Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        };
        assert_eq!(r.code, 0, "tcas {args:?}:\n{}\n{}", r.out, r.err);
        r
    }
}
