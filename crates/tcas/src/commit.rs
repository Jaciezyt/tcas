//! `tcas commit -m <message> [paths]` (daemon-and-cli.md section 5, "提交";
//! harness.md section 5.4): commits only the session's own changed files,
//! one commit per checkout, in path order. Not atomic: it stops at the
//! first checkout that fails.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tcas_core::human::{self};
use tcas_core::proto::{CommitDone, CommitPlan, CommitRepo, PathRef};

use crate::Ctx;

struct Made {
    repo: String,
    hash: Option<String>,
    shown: Vec<String>,
    error: Option<String>,
}

fn git(dir: &Path, args: &[&str]) -> (bool, String, String) {
    match Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// Runs a git command, retrying while another process holds the index lock.
fn git_retry(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let mut r = git(dir, args);
    for _ in 0..5 {
        if r.0 || !r.2.contains("index.lock") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        r = git(dir, args);
    }
    r
}

fn commit_one(r: &CommitRepo, message: &str) -> Result<String, String> {
    let dir = Path::new(&r.abs);
    let mut add: Vec<&str> = vec!["add", "--"];
    add.extend(r.paths.iter().map(String::as_str));
    let (ok, _, err) = git_retry(dir, &add);
    if !ok {
        return Err(format!("git add failed: {}", err.trim()));
    }
    let mut c: Vec<&str> = vec!["commit", "--only", "-q", "-m", message, "--"];
    c.extend(r.paths.iter().map(String::as_str));
    let (ok, out, err) = git_retry(dir, &c);
    if !ok {
        let why = [err.trim(), out.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n");
        return Err(if why.is_empty() { "git commit failed".into() } else { why });
    }
    let (ok, h, _) = git(dir, &["rev-parse", "HEAD"]);
    if !ok {
        return Err("committed, but HEAD cannot be read".into());
    }
    Ok(h.trim().to_string())
}

pub fn run(x: &Ctx, message: &str, paths: Vec<String>) -> Result<i32, i32> {
    if message.trim().is_empty() {
        return Err(x.fail("bad_request", "a commit message is required: tcas commit -m \"…\""));
    }
    let v = x.call("commit.prepare", json!({ "paths": paths }))?;
    let plan: CommitPlan =
        serde_json::from_value(v).map_err(|e| x.fail("internal", &format!("tcasd sent an unexpected answer: {e}")))?;

    let mut made: Vec<Made> = Vec::new();
    let mut released: Vec<PathRef> = Vec::new();
    let mut still: Vec<PathRef> = Vec::new();
    let mut failed = false;
    for r in &plan.repos {
        if failed {
            made.push(Made { repo: r.display.clone(), hash: None, shown: r.shown.clone(), error: Some("not committed (an earlier checkout failed)".into()) });
            continue;
        }
        match commit_one(r, message) {
            Ok(h) => {
                // The post-commit hook reported it already when installed;
                // reporting again is harmless (each commit counts once).
                let d = x.call("commit.done", json!({ "repo": r.abs, "hash": h, "paths": r.paths }))?;
                let d: CommitDone = serde_json::from_value(d).unwrap_or_default();
                released.extend(d.released);
                still = d.still_held;
                made.push(Made { repo: r.display.clone(), hash: Some(h), shown: r.shown.clone(), error: None });
            }
            Err(e) => {
                failed = true;
                made.push(Made { repo: r.display.clone(), hash: None, shown: r.shown.clone(), error: Some(e) });
            }
        }
    }

    if x.json {
        let repos: Vec<_> = made
            .iter()
            .map(|m| json!({ "repo": m.repo, "hash": m.hash, "paths": m.shown, "error": m.error }))
            .collect();
        crate::print_json(&json!({
            "repos": repos, "released": released, "still_held": still,
            "clean": plan.clean, "outside_repo": plan.outside_repo,
        }));
        return Ok(if failed { 1 } else { 0 });
    }

    if plan.repos.is_empty() {
        println!("nothing to commit: this session has no files with uncommitted changes{}.", if paths.is_empty() { "" } else { " among those" });
    }
    let w = made.iter().map(|m| m.repo.chars().count()).max().unwrap_or(0) + 3;
    for m in &made {
        let pad = " ".repeat(w.saturating_sub(m.repo.chars().count()));
        match (&m.hash, &m.error) {
            (Some(h), _) => {
                let n = human::count(m.shown.len(), "file", "files");
                println!("{}{pad}{}   {:<9}{}", m.repo, &h[..h.len().min(7)], n, m.shown.join(", "));
            }
            (None, Some(e)) => {
                println!("{}{pad}failed: {}", m.repo, e.lines().next().unwrap_or(""));
                for l in e.lines().skip(1).take(20) {
                    println!("{}{l}", " ".repeat(w));
                }
            }
            _ => {}
        }
    }
    if !plan.clean.is_empty() {
        println!("no changes to commit in: {}", plan.clean.join(", "));
    }
    if !plan.outside_repo.is_empty() {
        println!("not in any git checkout, not committed: {}", plan.outside_repo.join(", "));
    }
    if made.iter().any(|m| m.hash.is_some()) {
        let held: Vec<String> = still
            .iter()
            .map(|p| if p.is_dir { format!("{} (directory)", p.display) } else { p.display.clone() })
            .collect();
        let mut line = format!("released: {}.", human::count(released.len(), "file lease", "file leases"));
        if !held.is_empty() {
            line.push_str(&format!("  still held: {}", held.join(", ")));
        }
        println!("{line}");
        if !held.is_empty() {
            println!("When you are done with them:  tcas lease release <path>…   (or tcas lease release --all)");
        }
    }
    if failed {
        println!("Fix the failure and run the same command again; it commits only what is left.");
    }
    Ok(if failed { 1 } else { 0 })
}
