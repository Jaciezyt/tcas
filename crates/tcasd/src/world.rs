//! Workspaces as loaded for one request, and path resolution
//! (leases.md section 3).

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tcas_core::config::{Mode, WorkspaceConfig};
use tcas_core::exempt::{self, ExemptDir};
use tcas_core::proto::{PathRef, Unmanaged, UnmanagedReason};
use tcas_core::{abspath, relpath};
use tcas_sys::fsys;

use crate::git::Git;
use crate::repos;
use crate::store::{self, RepoRow, RootRow, WsRow};

#[derive(Debug, Clone)]
pub struct Ws {
    pub row: WsRow,
    pub roots: Vec<RootRow>,
    pub repos: Vec<RepoRow>,
    pub cfg: WorkspaceConfig,
    /// Exempt directories from the roots' `.tcas.toml`.
    pub exempt: Vec<ExemptDir>,
}

impl Ws {
    pub fn root(&self, name: &str) -> Option<&RootRow> {
        self.roots.iter().find(|r| r.name == name)
    }

    pub fn multi_root(&self) -> bool {
        self.roots.len() > 1
    }

    pub fn mode(&self, default: Mode) -> Mode {
        self.row.mode.as_deref().and_then(Mode::parse).or(self.cfg.mode).unwrap_or(default)
    }

    /// How a root-relative path is shown (leases.md section 3).
    pub fn display(&self, root: &str, rel: &str, is_dir: bool) -> String {
        if self.multi_root() {
            let p = relpath::join(root, rel);
            if is_dir { format!("{p}/") } else { p }
        } else {
            relpath::display(rel, is_dir)
        }
    }

    pub fn abs(&self, root: &str, rel: &str) -> String {
        let base = self.root(root).map(|r| r.path.clone()).unwrap_or_default();
        if rel.is_empty() { base } else { format!("{base}/{rel}") }
    }

    pub fn path_ref(&self, root: &str, rel: &str, is_dir: bool) -> PathRef {
        PathRef {
            root: root.to_string(),
            path: rel.to_string(),
            is_dir,
            display: self.display(root, rel, is_dir),
            abs: self.abs(root, rel),
        }
    }

    /// Repositories in a root, as (dir relative to the root, absolute path).
    pub fn repo_abs(&self, root: &str, dir: &str) -> PathBuf {
        PathBuf::from(self.abs(root, dir))
    }

    pub fn ci(&self, root: &str) -> bool {
        self.root(root).map(|r| r.ci).unwrap_or(false)
    }

    pub fn first_root_path(&self) -> &str {
        self.roots.first().map(|r| r.path.as_str()).unwrap_or("")
    }
}

#[derive(Debug, Clone, Default)]
pub struct World {
    pub wss: Vec<Ws>,
}

fn read_ws_cfg(root: &Path) -> (WorkspaceConfig, Option<String>) {
    match std::fs::read_to_string(root.join(".tcas.toml")) {
        Ok(text) => match tcas_core::config::parse_workspace(&text) {
            Ok(c) => (c, None),
            Err(e) => (WorkspaceConfig::default(), Some(format!("{}/.tcas.toml: {e}", root.display()))),
        },
        Err(_) => (WorkspaceConfig::default(), None),
    }
}

impl World {
    pub fn load(c: &Connection) -> rusqlite::Result<World> {
        let roots = store::roots(c)?;
        let mut wss = Vec::new();
        for row in store::workspaces(c)? {
            let rs: Vec<RootRow> = roots.iter().filter(|r| r.workspace == row.id).cloned().collect();
            let repos = store::repos(c, &row.id)?;
            let first = rs.first().map(|r| PathBuf::from(&r.path));
            let (cfg, _) = first.as_deref().map(read_ws_cfg).unwrap_or_default();
            let mut ex = Vec::new();
            for (i, r) in rs.iter().enumerate() {
                let entries = if i == 0 {
                    cfg.exempt.clone()
                } else {
                    read_ws_cfg(Path::new(&r.path)).0.exempt
                };
                let src = format!("workspace: {}/.tcas.toml", r.path);
                ex.extend(tcas_sys::exempt::workspace_list(Path::new(&r.path), &entries, &src, r.ci));
            }
            wss.push(Ws { row, roots: rs, repos, cfg, exempt: ex });
        }
        Ok(World { wss })
    }

    pub fn ws(&self, id: &str) -> Option<&Ws> {
        self.wss.iter().find(|w| w.row.id == id)
    }

    /// The root containing `abs`.
    pub fn root_of(&self, abs: &str) -> Option<(&Ws, &RootRow)> {
        for w in &self.wss {
            for r in &w.roots {
                if abspath::contains(&r.path, abs, r.ci) {
                    return Some((w, r));
                }
            }
        }
        None
    }

    /// `--workspace` value: id, name, or a path inside one of its roots.
    pub fn find(&self, spec: &str, cwd: &Path) -> Option<&Ws> {
        if let Some(w) = self.wss.iter().find(|w| w.row.id == spec || w.row.name == spec) {
            return Some(w);
        }
        let p = fsys::normalize(&fsys::absolute(cwd, Path::new(spec)));
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        self.root_of(&fsys::to_str(&p)).map(|(w, _)| w)
    }

    pub fn by_cwd(&self, cwd: &Path) -> Option<&Ws> {
        self.root_of(&fsys::to_str(cwd)).map(|(w, _)| w)
    }
}

/// A path after resolution.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub input: String,
    pub abs: String,
    pub is_dir: bool,
    pub managed: Option<Managed>,
    pub unmanaged: Option<UnmanagedReason>,
}

#[derive(Debug, Clone)]
pub struct Managed {
    pub ws: String,
    pub root: String,
    pub rel: String,
    pub key: String,
    pub ci: bool,
    /// The checkout owning the path, relative to the root.
    pub repo: Option<String>,
}

impl Resolved {
    pub fn unmanaged_view(&self) -> Option<Unmanaged> {
        self.unmanaged.clone().map(|reason| Unmanaged {
            input: self.input.clone(),
            abs: self.abs.clone(),
            is_dir: self.is_dir,
            reason,
        })
    }
}

fn nearest_existing(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    while !cur.exists() {
        if !cur.pop() {
            return PathBuf::from("/");
        }
    }
    cur
}

fn has_git_component(rel: &str, ci: bool) -> bool {
    rel.split('/').any(|c| if ci { c.eq_ignore_ascii_case(".git") } else { c == ".git" })
}

/// Resolves `input` (relative to `cwd`) through the steps of leases.md section 3.
pub fn resolve(world: &World, global_exempt: &[ExemptDir], git: &Git, cwd: &Path, input: &str) -> Resolved {
    let dir_hint = input.ends_with('/');
    let abs_path = fsys::normalize(&fsys::absolute(cwd, Path::new(input)));
    let abs = fsys::to_str(&abs_path);
    let is_dir = dir_hint || abs_path.is_dir();
    let mut r = Resolved { input: input.to_string(), abs: abs.clone(), is_dir, managed: None, unmanaged: None };

    if let Some(e) = exempt::find(global_exempt, &abs) {
        r.unmanaged = Some(UnmanagedReason::Exempt {
            dir: e.path.clone(),
            written: e.written.clone(),
            source: e.source.clone(),
        });
        return r;
    }
    let Some((ws, root)) = world.root_of(&abs) else {
        r.unmanaged = Some(UnmanagedReason::OutsideWorkspace);
        return r;
    };
    if let Some(e) = exempt::find(&ws.exempt, &abs) {
        r.unmanaged = Some(UnmanagedReason::Exempt {
            dir: e.path.clone(),
            written: e.written.clone(),
            source: e.source.clone(),
        });
        return r;
    }
    let rel = abspath::relative(&root.path, &abs);
    if has_git_component(&rel, root.ci) {
        r.unmanaged = Some(UnmanagedReason::GitDir);
        return r;
    }
    let start = if is_dir { nearest_existing(&abs_path) } else { nearest_existing(abs_path.parent().unwrap_or(&abs_path)) };
    let start_s = fsys::to_str(&start);
    let repo = if abspath::contains(&root.path, &start_s, root.ci) { repos::owning(&root.path, &start_s) } else { None };
    if let Some(repo_dir) = &repo {
        let in_repo = relpath::strip(repo_dir, &rel);
        if !in_repo.is_empty() {
            let repo_abs = ws.repo_abs(&root.name, repo_dir);
            let ex = git.check_ignore(&repo_abs, &[(in_repo.clone(), is_dir)]);
            if let Some(e) = ex.get(&in_repo) {
                r.unmanaged = Some(UnmanagedReason::GitExcluded {
                    source: e.source.clone(),
                    line: e.line,
                    pattern: e.pattern.clone(),
                });
                return r;
            }
        }
    }
    r.managed = Some(Managed {
        ws: ws.row.id.clone(),
        root: root.name.clone(),
        key: relpath::key(&rel, root.ci),
        rel,
        ci: root.ci,
        repo,
    });
    r
}

/// Folded form of a path relative to its checkout, for matching git output.
pub fn in_repo_key(m_rel: &str, repo: &str, ci: bool) -> String {
    relpath::key(&relpath::strip(repo, m_rel), ci)
}
