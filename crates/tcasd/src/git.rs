//! Read-only git queries, run in the checkout that owns the paths
//! (daemon-and-cli.md section 2, "git 查询").

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Git's per-repository variables (`git rev-parse --local-env-vars`, git 2.54).
/// Inherited from a git hook they would point every query at the wrong place.
pub const LOCAL_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Names to remove from a child's environment.
pub fn strip_list() -> Vec<String> {
    let mut out: Vec<String> = LOCAL_VARS.iter().map(|s| s.to_string()).collect();
    if let Ok(o) = Command::new("git").arg("rev-parse").arg("--local-env-vars").env_clear_git().output() {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let l = l.trim();
            if !l.is_empty() && !out.iter().any(|x| x == l) {
                out.push(l.to_string());
            }
        }
    }
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("GIT_CONFIG_KEY_") || k.starts_with("GIT_CONFIG_VALUE_")) && !out.iter().any(|x| x == &*k) {
            out.push(k.into_owned());
        }
    }
    out
}

trait ClearGit {
    fn env_clear_git(&mut self) -> &mut Self;
}

impl ClearGit for Command {
    fn env_clear_git(&mut self) -> &mut Self {
        for v in LOCAL_VARS {
            self.env_remove(v);
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct Git {
    strip: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub source: String,
    pub line: u32,
    pub pattern: String,
}

fn run_with_input(mut c: Command, input: &[u8]) -> Option<Output> {
    use std::io::Write;
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().ok();
    let _ = writer.join();
    out
}

fn pathspec(rel: &str, ci: bool) -> String {
    if ci { format!(":(literal,icase){rel}") } else { format!(":(literal){rel}") }
}

fn fold(s: &str, ci: bool) -> String {
    if ci { s.to_lowercase() } else { s.to_string() }
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    pub fn new() -> Git {
        Git { strip: strip_list() }
    }

    pub fn cmd(&self, dir: &Path) -> Command {
        let mut c = Command::new("git");
        c.arg("--no-optional-locks").arg("-C").arg(dir);
        for v in &self.strip {
            c.env_remove(v);
        }
        c.stdin(Stdio::null());
        c
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Option<Output> {
        self.cmd(dir).args(args).output().ok()
    }

    /// The top of the checkout containing `dir`, resolved.
    pub fn toplevel(&self, dir: &Path) -> Option<PathBuf> {
        let o = self.run(dir, &["rev-parse", "--show-toplevel"])?;
        if !o.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.is_empty() {
            return None;
        }
        std::fs::canonicalize(&s).ok().or(Some(PathBuf::from(s)))
    }

    pub fn common_dir(&self, repo: &Path) -> Option<String> {
        let o = self.run(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        if !o.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let p = std::fs::canonicalize(&s).unwrap_or_else(|_| PathBuf::from(&s));
        Some(p.to_string_lossy().into_owned())
    }

    /// `main`, or `(detached at 3f2a91c)`.
    pub fn branch(&self, repo: &Path) -> Option<String> {
        let o = self.run(repo, &["symbolic-ref", "-q", "--short", "HEAD"])?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !s.is_empty() {
            return Some(s);
        }
        let o = self.run(repo, &["rev-parse", "--short", "HEAD"])?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !s.is_empty() { Some(format!("(detached at {s})")) } else { None }
    }

    /// Which of `rels` (relative to `repo`; directories with `is_dir`) git
    /// excludes, with the rule. Tracked files are never excluded.
    pub fn check_ignore(&self, repo: &Path, rels: &[(String, bool)]) -> HashMap<String, Exclusion> {
        let mut out = HashMap::new();
        if rels.is_empty() {
            return out;
        }
        let mut c = self.cmd(repo);
        c.args(["check-ignore", "-v", "-z", "--stdin"]);
        let mut asked: HashMap<String, String> = HashMap::new();
        let mut input = Vec::new();
        for (r, is_dir) in rels {
            let q = if *is_dir && !r.is_empty() { format!("{r}/") } else { r.clone() };
            asked.insert(q.clone(), r.clone());
            input.extend_from_slice(q.as_bytes());
            input.push(0);
        }
        let Some(o) = run_with_input(c, &input) else { return out };
        let fields: Vec<&[u8]> = o.stdout.split(|b| *b == 0).collect();
        for rec in fields.chunks(4) {
            if rec.len() < 4 {
                break;
            }
            let pattern = String::from_utf8_lossy(rec[2]).into_owned();
            if pattern.starts_with('!') {
                continue;
            }
            let path = String::from_utf8_lossy(rec[3]).into_owned();
            let Some(orig) = asked.get(&path) else { continue };
            out.insert(
                orig.clone(),
                Exclusion {
                    source: String::from_utf8_lossy(rec[0]).into_owned(),
                    line: String::from_utf8_lossy(rec[1]).parse().unwrap_or(0),
                    pattern,
                },
            );
        }
        out
    }

    /// Paths with uncommitted changes, as folded keys relative to `repo`.
    /// With `rels` empty, the whole checkout.
    pub fn dirty(&self, repo: &Path, rels: &[String], ci: bool) -> HashSet<String> {
        let mut c = self.cmd(repo);
        c.args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"]);
        if !rels.is_empty() {
            c.arg("--");
            for r in rels {
                c.arg(pathspec(r, ci));
            }
        }
        let mut out = HashSet::new();
        let Ok(o) = c.output() else { return out };
        let mut it = o.stdout.split(|b| *b == 0);
        while let Some(rec) = it.next() {
            if rec.len() < 4 {
                continue;
            }
            let xy = &rec[..2];
            out.insert(fold(&String::from_utf8_lossy(&rec[3..]), ci));
            if (xy[0] == b'R' || xy[0] == b'C')
                && let Some(orig) = it.next()
            {
                out.insert(fold(&String::from_utf8_lossy(orig), ci));
            }
        }
        out
    }

    /// Like [`dirty`] for the whole checkout, keeping the spelling git uses.
    pub fn changed_paths(&self, repo: &Path) -> Vec<String> {
        let mut c = self.cmd(repo);
        c.args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"]);
        let mut out = Vec::new();
        let Ok(o) = c.output() else { return out };
        let mut it = o.stdout.split(|b| *b == 0);
        while let Some(rec) = it.next() {
            if rec.len() < 4 {
                continue;
            }
            let xy = &rec[..2];
            out.push(String::from_utf8_lossy(&rec[3..]).into_owned());
            if xy[0] == b'R' || xy[0] == b'C' {
                it.next();
            }
        }
        out
    }

    /// Tracked files under `rel_dir` ("" for the whole checkout), relative to `repo`.
    pub fn ls_files(&self, repo: &Path, rel_dir: &str, ci: bool) -> Vec<String> {
        let mut c = self.cmd(repo);
        c.args(["ls-files", "-z"]);
        if !rel_dir.is_empty() {
            c.arg("--").arg(pathspec(&format!("{rel_dir}/"), ci));
        }
        let Ok(o) = c.output() else { return vec![] };
        o.stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }

    /// Directories git excludes in this checkout, relative to `repo`, without
    /// a trailing slash. `ls-files --directory` also lists directories whose
    /// contents are all ignored (`sub/` holding only `sub/build/`), so each
    /// candidate is confirmed with check-ignore.
    pub fn excluded_dirs(&self, repo: &Path) -> HashSet<String> {
        let Some(o) = self.run(repo, &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"])
        else {
            return HashSet::new();
        };
        let cands: Vec<(String, bool)> = o
            .stdout
            .split(|b| *b == 0)
            .filter(|s| s.ends_with(b"/"))
            .map(|s| (String::from_utf8_lossy(&s[..s.len() - 1]).into_owned(), true))
            .collect();
        let ex = self.check_ignore(repo, &cands);
        let mut out: HashSet<String> = ex.into_keys().collect();
        // A listed directory that is not itself excluded may still hold
        // excluded subdirectories; ask for those one level at a time.
        for (d, _) in &cands {
            if out.contains(d) {
                continue;
            }
            let sub = self.excluded_below(repo, d);
            out.extend(sub);
        }
        out
    }

    fn excluded_below(&self, repo: &Path, dir: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let Ok(rd) = std::fs::read_dir(repo.join(dir)) else { return out };
        let subs: Vec<(String, bool)> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| (format!("{dir}/{}", e.file_name().to_string_lossy()), true))
            .collect();
        let ex = self.check_ignore(repo, &subs);
        for (s, _) in subs {
            if ex.contains_key(&s) {
                out.insert(s);
            } else if s.matches('/').count() < 8 {
                out.extend(self.excluded_below(repo, &s));
            }
        }
        out
    }

    /// Checkouts of this repository: (directory, branch, is_bare).
    pub fn worktrees(&self, repo: &Path) -> Vec<(PathBuf, Option<String>)> {
        let Some(o) = self.run(repo, &["worktree", "list", "--porcelain", "-z"]) else { return vec![] };
        let mut out = Vec::new();
        let mut cur: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut skip = false;
        let mut flush = |cur: &mut Option<PathBuf>, branch: &mut Option<String>, skip: &mut bool| {
            if let Some(p) = cur.take()
                && !*skip
            {
                out.push((p, branch.take()));
            }
            *branch = None;
            *skip = false;
        };
        for f in o.stdout.split(|b| *b == 0) {
            let line = String::from_utf8_lossy(f);
            if line.is_empty() {
                flush(&mut cur, &mut branch, &mut skip);
            } else if let Some(p) = line.strip_prefix("worktree ") {
                flush(&mut cur, &mut branch, &mut skip);
                cur = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.trim_start_matches("refs/heads/").to_string());
            } else if line == "bare" || line.starts_with("prunable") {
                skip = true;
            }
        }
        flush(&mut cur, &mut branch, &mut skip);
        out
    }
}
