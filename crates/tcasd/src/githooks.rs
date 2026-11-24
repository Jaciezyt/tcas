//! Installing tcas's git hooks (harness.md section 5.6): one file of hook
//! definitions in the tcas home, included by one line in each repository's
//! `.git/config`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tcas_core::pathfmt::shell_quote;
use tcas_sys::fsys;
use tcas_sys::home::Home;

use crate::git::Git;

/// The hooks, in the order they are written.
pub const HOOKS: &[&str] =
    &["pre-commit", "post-commit", "pre-rebase", "reference-transaction", "post-checkout", "post-merge", "post-rewrite"];

/// The `tcas` program the hooks run: the one next to this program (both are
/// built together), else the plugin's forwarder in the home.
pub fn tcas_program(home: &Home) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sib = exe.with_file_name("tcas");
        if sib.is_file() {
            return std::fs::canonicalize(&sib).unwrap_or(sib);
        }
    }
    home.dir.join("bin").join("tcas")
}

/// The hook command for `name`. A home other than the default is passed
/// on, so the hooks talk to the tcasd that installed them.
fn command(home: &Home, name: &str) -> String {
    let prog = shell_quote(&fsys::to_str(&tcas_program(home)));
    if home.is_custom() {
        format!("TCAS_HOME={} {prog} git-hook {name}", shell_quote(&fsys::to_str(&home.dir)))
    } else {
        format!("{prog} git-hook {name}")
    }
}

/// gitconfig quoting for a value.
fn quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn config_text(home: &Home) -> String {
    let mut out = String::from("# Written by tcas (harness.md section 5.6). Do not edit; tcas rewrites it.\n");
    for h in HOOKS {
        out.push_str(&format!(
            "[hook \"tcas-{h}\"]\n\tevent = {h}\n\tcommand = {}\n",
            quote(&command(home, h))
        ));
    }
    out
}

/// Writes the definitions file when it differs.
pub fn write_config(home: &Home) -> std::io::Result<()> {
    let p = home.git_hooks_config();
    let text = config_text(home);
    if std::fs::read_to_string(&p).ok().as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    let tmp = p.with_extension("gitconfig.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &p)
}

/// git runs hooks defined in its configuration: tried once in a temporary
/// repository, not judged by the version.
pub fn supported(git: &Git) -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| probe(git))
}

fn probe(git: &Git) -> bool {
    let dir = std::env::temp_dir().join(format!("tcas-hook-probe-{}-{}", std::process::id(), tcas_sys::clock::now_ms()));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let mark = dir.join("ran");
    let ok = git.cmd(&dir).args(["init", "-q"]).output().is_ok_and(|o| o.status.success())
        && git
            .cmd(&dir)
            .args([
                "-c",
                "hook.tcas-probe.event=pre-commit",
                "-c",
                &format!("hook.tcas-probe.command=touch {}", shell_quote(&fsys::to_str(&mark))),
                "hook",
                "run",
                "pre-commit",
            ])
            .output()
            .is_ok()
        && mark.exists();
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

/// The checkout's own config file holds the include line.
pub fn installed(git: &Git, home: &Home, repo: &Path) -> bool {
    let p = fsys::to_str(&home.git_hooks_config());
    let Ok(o) = git.cmd(repo).args(["config", "--local", "--get-all", "include.path"]).output() else { return false };
    String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == p)
}

pub fn install(git: &Git, home: &Home, repo: &Path) -> Result<bool, String> {
    if installed(git, home, repo) {
        return Ok(false);
    }
    write_config(home).map_err(|e| format!("{}: {e}", home.git_hooks_config().display()))?;
    let p = fsys::to_str(&home.git_hooks_config());
    let o = git
        .cmd(repo)
        .args(["config", "--local", "--add", "include.path", &p])
        .output()
        .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(true)
}

/// Removes only tcas's own line.
pub fn remove(git: &Git, home: &Home, repo: &Path) -> Result<bool, String> {
    if !installed(git, home, repo) {
        return Ok(false);
    }
    let p = fsys::to_str(&home.git_hooks_config());
    let o = git
        .cmd(repo)
        .args(["config", "--local", "--fixed-value", "--unset-all", "include.path", &p])
        .output()
        .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(true)
}
