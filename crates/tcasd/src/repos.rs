//! Finding the checkouts in a workspace root (leases.md section 3, "多个仓库").

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tcas_core::abspath;
use tcas_core::exempt::{self, ExemptDir};

use crate::git::Git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Relative to the root; "" when the root is the checkout.
    pub dir: String,
    pub kind: String,
    pub common_dir: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub found: Vec<Found>,
    /// Linked worktrees of these repositories that lie outside the given roots.
    pub outside: Vec<(PathBuf, Option<String>)>,
}

/// `repo`, `submodule` or `worktree`, from what `.git` is.
pub fn kind_of(dir: &Path) -> Option<&'static str> {
    let dot = dir.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    if meta.is_dir() {
        return Some("repo");
    }
    if meta.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim().to_string();
        return Some(if target.contains("/worktrees/") { "worktree" } else { "submodule" });
    }
    None
}

fn rel(root: &str, abs: &Path) -> String {
    abspath::relative(root, &abs.to_string_lossy())
}

/// Scans `root` down to `depth` levels, then asks each repository for its
/// linked worktrees. `all_roots` decides what counts as outside.
pub fn discover(
    git: &Git,
    root: &Path,
    ci: bool,
    depth: usize,
    exempt_list: &[ExemptDir],
    all_roots: &[(String, bool)],
) -> Discovery {
    let root_s = root.to_string_lossy().into_owned();
    let mut checkouts: Vec<(PathBuf, &'static str)> = Vec::new();
    // (directory, depth, the innermost enclosing checkout and its excluded dirs)
    let mut stack: Vec<(PathBuf, usize, Option<(PathBuf, std::rc::Rc<HashSet<String>>)>)> =
        vec![(root.to_path_buf(), 0, None)];
    while let Some((dir, d, enclosing)) = stack.pop() {
        if exempt::find(exempt_list, &dir.to_string_lossy()).is_some() {
            continue;
        }
        let mut enclosing = enclosing;
        if let Some(k) = kind_of(&dir) {
            checkouts.push((dir.clone(), k));
            enclosing = Some((dir.clone(), std::rc::Rc::new(git.excluded_dirs(&dir))));
        }
        if d >= depth {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut children: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| e.file_name() != ".git")
            .map(|e| e.path())
            .collect();
        children.sort();
        for c in children.into_iter().rev() {
            if let Some((repo, ex)) = &enclosing {
                let r = abspath::relative(&repo.to_string_lossy(), &c.to_string_lossy());
                if ex.contains(&r) {
                    continue;
                }
            }
            stack.push((c, d + 1, enclosing.clone()));
        }
    }

    let mut found: Vec<Found> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut outside = Vec::new();
    let mut outside_seen: HashSet<String> = HashSet::new();
    let mut extra: Vec<(PathBuf, Option<String>)> = Vec::new();
    for (dir, kind) in &checkouts {
        let r = rel(&root_s, dir);
        seen.insert(if ci { r.to_lowercase() } else { r.clone() });
        found.push(Found {
            dir: r,
            kind: kind.to_string(),
            common_dir: git.common_dir(dir),
            branch: git.branch(dir),
        });
        if *kind == "submodule" {
            continue;
        }
        for (wt, branch) in git.worktrees(dir) {
            let wt = std::fs::canonicalize(&wt).unwrap_or(wt);
            let ws = wt.to_string_lossy().into_owned();
            if exempt::find(exempt_list, &ws).is_some() {
                continue;
            }
            if abspath::contains(&root_s, &ws, ci) {
                extra.push((wt, branch));
            } else if !all_roots.iter().any(|(p, rci)| abspath::contains(p, &ws, *rci))
                && outside_seen.insert(ws.clone())
            {
                outside.push((wt, branch));
            }
        }
    }
    for (wt, branch) in extra {
        let r = rel(&root_s, &wt);
        let k = if ci { r.to_lowercase() } else { r.clone() };
        if !seen.insert(k) || !wt.exists() {
            continue;
        }
        let kind = kind_of(&wt).unwrap_or("worktree");
        found.push(Found { dir: r, kind: kind.to_string(), common_dir: git.common_dir(&wt), branch });
    }
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    Discovery { found, outside }
}

/// The checkout owning `abs`: the nearest directory at or above `start`
/// (inside `root`) that holds `.git`. Returns it relative to the root.
pub fn owning(root: &str, start: &str) -> Option<String> {
    let mut cur = PathBuf::from(start);
    loop {
        let s = cur.to_string_lossy().into_owned();
        if s.len() < root.len() {
            return None;
        }
        if cur.join(".git").exists() || std::fs::symlink_metadata(cur.join(".git")).is_ok() {
            return Some(abspath::relative(root, &s));
        }
        if s.len() == root.len() {
            return None;
        }
        if !cur.pop() {
            return None;
        }
    }
}
