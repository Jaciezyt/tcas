//! Configuration files (daemon-and-cli.md section 6). Parsing only; reading the
//! files is the caller's job.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Strict,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Strict => "strict",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "strict" => Some(Mode::Strict),
            _ => None,
        }
    }
}

/// `~/.tcas/config.toml`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub daemon: DaemonCfg,
    pub defaults: DefaultsCfg,
    pub exempt: ExemptCfg,
    pub agents: AgentsCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonCfg {
    pub liveness_interval_secs: u64,
    pub event_retention_days: u64,
}

impl Default for DaemonCfg {
    fn default() -> Self {
        DaemonCfg { liveness_interval_secs: 15, event_retention_days: 30 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DefaultsCfg {
    pub mode: Mode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExemptCfg {
    pub builtin: bool,
    pub paths: Vec<String>,
}

impl Default for ExemptCfg {
    fn default() -> Self {
        ExemptCfg { builtin: true, paths: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentsCfg {
    pub workspace_open: bool,
    pub open_max_checkouts: usize,
}

impl Default for AgentsCfg {
    fn default() -> Self {
        AgentsCfg { workspace_open: true, open_max_checkouts: 8 }
    }
}

/// `.tcas.toml` in the first root of a workspace.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub mode: Option<Mode>,
    pub roots: Vec<String>,
    pub hint_outside: bool,
    pub exempt: Vec<String>,
    pub unmanaged: Vec<String>,
    pub repos: ReposCfg,
    pub bash: BashCfg,
    pub git_hooks: GitHooksCfg,
    pub lease: LeaseCfg,
    pub brief: BriefCfg,
    pub wait: WaitCfg,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        WorkspaceConfig {
            mode: None,
            roots: Vec::new(),
            hint_outside: true,
            exempt: Vec::new(),
            unmanaged: Vec::new(),
            repos: ReposCfg::default(),
            bash: BashCfg::default(),
            git_hooks: GitHooksCfg::default(),
            lease: LeaseCfg::default(),
            brief: BriefCfg::default(),
            wait: WaitCfg::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReposCfg {
    pub scan_depth: usize,
}

impl Default for ReposCfg {
    fn default() -> Self {
        ReposCfg { scan_depth: 4 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BashCfg {
    pub reconcile: bool,
    pub git_guard: bool,
    pub approve_tcas: bool,
    pub background_window_secs: u64,
}

impl Default for BashCfg {
    fn default() -> Self {
        BashCfg { reconcile: true, git_guard: true, approve_tcas: true, background_window_secs: 60 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GitHooksCfg {
    pub enabled: bool,
    pub guard_stash: bool,
    pub check_user: bool,
}

impl Default for GitHooksCfg {
    fn default() -> Self {
        GitHooksCfg { enabled: true, guard_stash: true, check_user: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LeaseCfg {
    pub explicit_idle_secs: u64,
    pub waited_grace_secs: u64,
    pub stalled_after_secs: u64,
    pub max_dir_files: u64,
}

impl Default for LeaseCfg {
    fn default() -> Self {
        LeaseCfg {
            explicit_idle_secs: 1800,
            waited_grace_secs: 600,
            stalled_after_secs: 1800,
            max_dir_files: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BriefCfg {
    pub require: bool,
    pub broadcast: bool,
    pub max_chars: usize,
    pub hint_after_files: usize,
}

impl Default for BriefCfg {
    fn default() -> Self {
        BriefCfg { require: false, broadcast: true, max_chars: 200, hint_after_files: 3 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WaitCfg {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
}

impl Default for WaitCfg {
    fn default() -> Self {
        WaitCfg { default_timeout_secs: 1800, max_timeout_secs: 14400 }
    }
}

pub fn parse_global(text: &str) -> Result<GlobalConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

pub fn parse_workspace(text: &str) -> Result<WorkspaceConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let g = parse_global("").unwrap();
        assert!(g.exempt.builtin);
        assert_eq!(g.daemon.liveness_interval_secs, 15);
        let w = parse_workspace("").unwrap();
        assert_eq!(w.lease.max_dir_files, 200);
        assert_eq!(w.mode, None);
    }

    #[test]
    fn partial_sections() {
        let w = parse_workspace("mode = \"strict\"\n[lease]\nexplicit_idle_secs = 2\n").unwrap();
        assert_eq!(w.mode, Some(Mode::Strict));
        assert_eq!(w.lease.explicit_idle_secs, 2);
        assert_eq!(w.lease.waited_grace_secs, 600);
        let g = parse_global("[exempt]\nbuiltin = false\npaths = [\"~/scratch\"]\n").unwrap();
        assert!(!g.exempt.builtin);
        assert_eq!(g.exempt.paths, vec!["~/scratch"]);
    }
}
