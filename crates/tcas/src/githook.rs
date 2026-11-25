//! `tcas git-hook <name> [args]`: run by git (harness.md section 5.6).
//! A checkout outside every workspace is let through at once, without
//! tcasd. When tcasd cannot be reached the operation goes ahead with a
//! warning: git is the user's tool too.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tcas_core::proto::{GitHookAnswer, GitHookArgs};
use tcas_sys::fsys;

use crate::client::Client;
use crate::hook::Local;

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git(args: &[&str]) -> Option<Vec<u8>> {
    // The hook's own environment: `git commit --only` passes a temporary
    // index in GIT_INDEX_FILE.
    let o = Command::new("git").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then_some(o.stdout)
}

fn names(out: Vec<u8>) -> Vec<String> {
    out.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

fn stdin_text() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().take(16 << 20).read_to_string(&mut s);
    s
}

pub fn run(name: &str, args: &[String]) -> i32 {
    let mut hook = name.to_string();
    // Cheap exits first: reference-transaction runs on every ref update.
    match name {
        "reference-transaction" => {
            let input = stdin_text();
            if args.first().map(String::as_str) != Some("prepared") {
                return 0;
            }
            let new: Vec<&str> = input
                .lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    (f.len() == 3 && f[2] == "refs/stash" && f[1] != ZERO && !f[1].chars().all(|c| c == '0')).then_some(f[1])
                })
                .collect();
            if new.is_empty() {
                return 0;
            }
            let known = git(&["rev-list", "-g", "refs/stash"]).map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
            if new.iter().all(|n| known.lines().any(|k| k.trim() == *n)) {
                return 0;
            }
            hook = "stash".into();
        }
        "post-rewrite" => {
            let _ = stdin_text();
        }
        // `post-checkout` reports a branch switch (third argument 1), not a
        // file restore (harness.md section 5.6).
        "post-checkout" if args.get(2).map(String::as_str) != Some("1") => return 0,
        "pre-commit" | "post-commit" | "pre-rebase" | "post-checkout" | "post-merge" => {}
        _ => return 0,
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let cwd_s = fsys::to_str(&cwd);
    let local = Local::load();
    if local.root_of(&cwd_s).is_none() || local.is_exempt(&cwd_s) {
        return 0;
    }

    let mut a = GitHookArgs { hook: hook.clone(), repo: cwd_s.clone(), args: args.to_vec(), ..GitHookArgs::default() };
    a.allow = std::env::var("TCAS_ALLOW").is_ok_and(|v| !v.is_empty() && v != "0");
    match hook.as_str() {
        "pre-commit" => {
            let Some(out) = git(&["diff", "--cached", "--name-only", "-z", "--no-renames"]) else { return 0 };
            a.paths = names(out);
            if a.paths.is_empty() {
                return 0;
            }
        }
        "post-commit" => {
            let Some(h) = git(&["rev-parse", "HEAD"]) else { return 0 };
            let h = String::from_utf8_lossy(&h).trim().to_string();
            a.paths = git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "--root", "--no-renames", &h, "--"])
                .map(names)
                .unwrap_or_default();
            a.hash = Some(h);
        }
        _ => {}
    }

    let mut c = Client::from_env();
    c.cwd = cwd;
    let answer = (|| -> Result<GitHookAnswer, String> {
        let mut s = c.ensure(Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        Client::send(&mut s, &c.request("git.hook", json!(a)))?;
        let r = Client::receive(&s)?.ok_or("tcasd closed the connection without answering")?;
        if !r.ok {
            return Err(r.error.map(|e| e.message).unwrap_or_else(|| "tcasd gave no reason".into()));
        }
        serde_json::from_value(r.result.unwrap_or_default()).map_err(|e| e.to_string())
    })();
    match answer {
        Ok(ans) => {
            if let Some(m) = ans.message.filter(|m| !m.is_empty()) {
                eprintln!("{m}");
            }
            if ans.allow { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("tcas: warning: {name} hook not checked ({e}).");
            0
        }
    }
}
