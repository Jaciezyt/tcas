//! Where tcas keeps its files (daemon-and-cli.md section 1).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    /// `$TCAS_HOME`, or `~/.tcas`.
    pub fn from_env() -> Home {
        let dir = match std::env::var_os("TCAS_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => user_home().unwrap_or_else(|| PathBuf::from("/tmp")).join(".tcas"),
        };
        Home { dir }
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("tcasd.sock")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.dir.join("tcasd.pid")
    }
    pub fn db(&self) -> PathBuf {
        self.dir.join("tcas.db")
    }
    pub fn workspaces_json(&self) -> PathBuf {
        self.dir.join("workspaces.json")
    }
    pub fn config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
    pub fn log(&self) -> PathBuf {
        self.dir.join("tcasd.log")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sessions")
    }
    /// Exists while tcasd has a record of the session, so `tcas hook` keeps
    /// reporting a session whose directory is outside every workspace.
    pub fn session_marker(&self, sid: &str) -> PathBuf {
        let name: String =
            sid.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
        let name = if name.starts_with('.') { format!("_{name}") } else { name };
        self.sessions_dir().join(name)
    }
    pub fn plugins_dir(&self) -> PathBuf {
        self.dir.join("plugins")
    }
    /// The git hook definitions each repository includes (harness.md section 5.6).
    pub fn git_hooks_config(&self) -> PathBuf {
        self.dir.join("git-hooks.gitconfig")
    }
    /// The home given by `TCAS_HOME`, not the default one.
    pub fn is_custom(&self) -> bool {
        user_home().is_none_or(|h| h.join(".tcas") != self.dir)
    }

    /// Creates the directory with mode 0700.
    pub fn ensure(&self) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !self.dir.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        }
        let meta = std::fs::metadata(&self.dir)?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

pub fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}
