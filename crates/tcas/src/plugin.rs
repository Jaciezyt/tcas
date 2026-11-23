//! Harness plugins (harness.md section 1). The files are built into the
//! program and written to `~/.tcas/plugins/<harness>/` on demand, so the
//! plugin always matches the installed `tcas`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tcas_sys::home::Home;

struct File {
    path: &'static str,
    text: &'static str,
    exec: bool,
}

const CLAUDE: &[File] = &[
    File {
        path: ".claude-plugin/plugin.json",
        text: include_str!("../../../plugins/claude/.claude-plugin/plugin.json"),
        exec: false,
    },
    File { path: "hooks/hooks.json", text: include_str!("../../../plugins/claude/hooks/hooks.json"), exec: false },
    File { path: "bin/tcas", text: include_str!("../../../plugins/claude/bin/tcas"), exec: true },
];

fn files(harness: &str) -> Option<&'static [File]> {
    match harness {
        "claude" => Some(CLAUDE),
        _ => None,
    }
}

/// Writes the plugin for `harness` under the tcas home and returns its directory.
pub fn write(home: &Home, harness: &str) -> Result<PathBuf, String> {
    let list = files(harness).ok_or_else(|| format!("unknown harness {harness:?} (known: claude)"))?;
    home.ensure().map_err(|e| format!("{}: {e}", home.dir.display()))?;
    let dir = home.plugins_dir().join(harness);
    let me = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
    for f in list {
        let p = dir.join(f.path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut text = f.text.to_string();
        if f.path == "bin/tcas"
            && let Some(me) = &me
        {
            let q = tcas_core::pathfmt::shell_quote(&me.to_string_lossy());
            text = text.replace("TCAS_BUILT=\"\"", &format!("TCAS_BUILT={q}"));
        }
        write_if_changed(&p, &text)?;
        if f.exec {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    Ok(dir)
}

fn write_if_changed(p: &Path, text: &str) -> Result<(), String> {
    if std::fs::read_to_string(p).is_ok_and(|t| t == text) {
        return Ok(());
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn path(harness: &str, json: bool) -> i32 {
    match write(&Home::from_env(), harness) {
        Ok(dir) => {
            if json {
                println!("{}", serde_json::json!({ "harness": harness, "path": dir }));
            } else {
                println!("{}", dir.display());
            }
            0
        }
        Err(e) => {
            eprintln!("tcas: {e}");
            1
        }
    }
}
