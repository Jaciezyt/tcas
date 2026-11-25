//! Harness plugins (harness.md sections 1 and 10). The files are built into
//! the program and written under `~/.tcas/plugins/` on demand, so the plugin
//! always matches the installed `tcas`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tcas_core::proto::code;
use tcas_sys::home::{Home, user_home};

use crate::Ctx;
use crate::client::{self, Client};

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
    File { path: "skills/tcas/SKILL.md", text: include_str!("../../../plugins/claude/skills/tcas/SKILL.md"), exec: false },
    File { path: "bin/tcas", text: include_str!("../../../plugins/claude/bin/tcas"), exec: true },
];

const AGY: &[File] = &[
    File { path: "plugin.json", text: include_str!("../../../plugins/agy/plugin.json"), exec: false },
    File { path: "hooks.json", text: include_str!("../../../plugins/agy/hooks.json"), exec: false },
    File { path: "skills/tcas/SKILL.md", text: include_str!("../../../plugins/agy/skills/tcas/SKILL.md"), exec: false },
    File { path: "bin/tcas", text: include_str!("../../../plugins/agy/bin/tcas"), exec: true },
];

fn spec(harness: &str) -> Option<(&'static str, &'static [File])> {
    match harness {
        "claude" => Some(("claude", CLAUDE)),
        // Named `tcas` so `agy plugin install ~/.tcas/plugins/tcas` installs it
        // under plugin ID `tcas` (harness.md section 10).
        "agy" => Some(("tcas", AGY)),
        _ => None,
    }
}

pub fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0).unwrap_or(false)
}

/// Writes the plugin for `harness` under the tcas home and returns its directory.
pub fn write(home: &Home, harness: &str) -> Result<PathBuf, String> {
    let (subdir, list) = spec(harness).ok_or_else(|| format!("unknown harness {harness:?} (known: claude, agy)"))?;
    home.ensure().map_err(|e| format!("{}: {e}", home.dir.display()))?;
    let dir = home.plugins_dir().join(subdir);
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
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("{}: {e}", p.display()))?;
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
                crate::print_json(&serde_json::json!({ "harness": harness, "path": dir }));
            } else {
                println!("{}", dir.display());
            }
            0
        }
        Err(e) => {
            if json {
                crate::print_json(&serde_json::json!({ "ok": false, "error": { "code": "bad_request", "message": e } }));
            } else {
                eprintln!("tcas: {e}");
            }
            1
        }
    }
}

pub struct ClaudePluginState {
    pub plugin_dir: PathBuf,
    pub plugin_files_ok: bool,
    pub marketplace_installed: bool,
    pub plugin_installed: bool,
    pub plugin_enabled: bool,
    pub installed_version: Option<String>,
    pub cache_bin_executable: Option<bool>,
}

pub struct AgyPluginState {
    pub plugin_dir: PathBuf,
    pub plugin_files_ok: bool,
    pub installed_dir: Option<PathBuf>,
    pub plugin_installed: bool,
    pub plugin_enabled: bool,
    pub installed_version: Option<String>,
    pub installed_hooks_ok: Option<bool>,
    pub installed_bin_executable: Option<bool>,
}

fn read_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok())
}

pub fn claude_state(home: &Home) -> ClaudePluginState {
    let plugin_dir = home.plugins_dir().join("claude");
    let plugin_files_ok = CLAUDE.iter().all(|f| {
        let p = plugin_dir.join(f.path);
        p.is_file() && (!f.exec || is_executable(&p))
    });
    let claude_dir = user_home().map(|h| h.join(".claude"));
    let settings = claude_dir.as_ref().and_then(|d| read_json(&d.join("settings.json")));
    let known_mkts = claude_dir.as_ref().and_then(|d| read_json(&d.join("plugins/known_marketplaces.json")));
    let installed_file = claude_dir.as_ref().and_then(|d| read_json(&d.join("plugins/installed_plugins.json")));

    let marketplace_installed = settings
        .as_ref()
        .and_then(|v| v.get("extraKnownMarketplaces"))
        .and_then(|v| v.get("tcas"))
        .is_some()
        || known_mkts.as_ref().and_then(|v| v.get("tcas")).is_some();

    let entry = installed_file
        .as_ref()
        .and_then(|v| v.get("plugins"))
        .and_then(|v| v.get("tcas@tcas"))
        .and_then(Value::as_array)
        .and_then(|a| a.first());

    let plugin_installed = entry.is_some();
    let installed_version = entry.and_then(|e| e.get("version")).and_then(Value::as_str).map(str::to_string);
    let cache_bin_executable = entry
        .and_then(|e| e.get("installPath"))
        .and_then(Value::as_str)
        .map(|p| is_executable(&Path::new(p).join("bin/tcas")));

    let plugin_enabled = settings
        .as_ref()
        .and_then(|v| v.get("enabledPlugins"))
        .and_then(|v| v.get("tcas@tcas"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    ClaudePluginState {
        plugin_dir,
        plugin_files_ok,
        marketplace_installed,
        plugin_installed,
        plugin_enabled,
        installed_version,
        cache_bin_executable,
    }
}

pub fn agy_state(home: &Home) -> AgyPluginState {
    let plugin_dir = home.plugins_dir().join("tcas");
    let plugin_files_ok = AGY.iter().all(|f| {
        let p = plugin_dir.join(f.path);
        p.is_file() && (!f.exec || is_executable(&p))
    });
    let cfg_dir = user_home().map(|h| h.join(".gemini/config"));
    let installed_dir = cfg_dir.as_ref().map(|d| d.join("plugins/tcas"));
    let manifest = installed_dir.as_ref().and_then(|d| read_json(&d.join("plugin.json")));
    let plugin_installed = manifest.is_some();
    let installed_version = manifest.as_ref().and_then(|m| m.get("version")).and_then(Value::as_str).map(str::to_string);
    let installed_hooks_ok = if plugin_installed {
        installed_dir.as_ref().map(|d| d.join("hooks.json").is_file())
    } else {
        None
    };
    let installed_bin_executable = if plugin_installed {
        installed_dir.as_ref().map(|d| is_executable(&d.join("bin/tcas")))
    } else {
        None
    };

    let config_json = cfg_dir.as_ref().and_then(|d| read_json(&d.join("config.json")));
    let default_enabled = !manifest
        .as_ref()
        .and_then(|m| m.get("disabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let plugin_enabled = if plugin_installed {
        config_json
            .as_ref()
            .and_then(|c| c.get("plugins"))
            .and_then(|p| p.get("tcas"))
            .and_then(|t| t.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(default_enabled)
    } else {
        false
    };

    AgyPluginState {
        plugin_dir,
        plugin_files_ok,
        installed_dir,
        plugin_installed,
        plugin_enabled,
        installed_version,
        installed_hooks_ok,
        installed_bin_executable,
    }
}

pub fn warn_if_missing(home: &Home) {
    let has_claude = user_home().is_some_and(|h| h.join(".claude").is_dir())
        || Command::new("claude")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
    if has_claude {
        let st = claude_state(home);
        if !st.plugin_installed {
            println!("note: Claude Code plugin tcas is not installed. Run: tcas plugin install claude");
        } else if !st.plugin_enabled {
            println!("note: Claude Code plugin tcas is installed but disabled. Run: tcas plugin install claude");
        }
    }

    let has_agy = user_home().is_some_and(|h| h.join(".gemini").is_dir())
        || Command::new("agy")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
    if has_agy {
        let st = agy_state(home);
        if !st.plugin_installed {
            println!("note: AGY plugin tcas is not installed. Run: tcas plugin install agy");
        } else if !st.plugin_enabled {
            println!("note: AGY plugin tcas is installed but disabled. Run: tcas plugin install agy");
        }
    }
}

fn check_user_only(x: &Ctx) -> Result<(), i32> {
    let msg = "tcas plugin install is only for the user. This is for the user to do: tell them what you need.";
    if client::session_from_env().is_some() {
        return Err(x.fail(code::USER_ONLY, msg));
    }
    if let Ok(mut s) = x.c.connect()
        && Client::send(&mut s, &x.c.request("whoami", json!({}))).is_ok()
        && let Ok(Some(r)) = Client::receive(&s)
        && r.ok
        && r.result.as_ref().and_then(|v| v["user"].as_bool()) == Some(false)
    {
        return Err(x.fail(code::USER_ONLY, msg));
    }
    Ok(())
}

fn run_cli(prog: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{prog} CLI not found on PATH")
            } else {
                format!("cannot run {prog}: {e}")
            }
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        return Err(format!("{prog} {} failed: {detail}", args.join(" ")));
    }
    Ok(stdout)
}

fn same_dir(a: &Path, b: &str) -> bool {
    let pb = Path::new(b);
    match (std::fs::canonicalize(a), std::fs::canonicalize(pb)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == pb,
    }
}

fn write_marketplace_manifest(mkt_dir: &Path) -> Result<(), String> {
    let meta = mkt_dir.join(".claude-plugin");
    std::fs::create_dir_all(&meta).map_err(|e| format!("{}: {e}", meta.display()))?;
    let manifest = json!({
        "name": "tcas",
        "owner": { "name": "tcas" },
        "plugins": [
            {
                "name": "tcas",
                "description": "Leases on files and directories, so several Claude Code sessions can share a checkout without overwriting each other's work.",
                "version": env!("CARGO_PKG_VERSION"),
                "source": "./claude"
            }
        ]
    });
    let text = format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap());
    write_if_changed(&meta.join("marketplace.json"), &text)
}

pub fn install(x: &Ctx, harness: &str) -> Result<i32, i32> {
    check_user_only(x)?;
    match harness {
        "claude" => install_claude(x),
        "agy" => install_agy(x),
        _ => Err(x.fail(code::BAD_REQUEST, &format!("unknown harness {harness:?} (known: claude, agy)"))),
    }
}

fn install_claude(x: &Ctx) -> Result<i32, i32> {
    let plugin_dir = write(&x.c.home, "claude").map_err(|e| x.fail(code::INTERNAL, &e))?;
    let mkt_dir = x.c.home.plugins_dir();
    write_marketplace_manifest(&mkt_dir).map_err(|e| x.fail(code::INTERNAL, &e))?;
    let mkt_str = mkt_dir.to_string_lossy().into_owned();

    let mlist_raw = run_cli("claude", &["plugin", "marketplace", "list", "--json"]).map_err(|e| x.fail(code::REFUSED, &e))?;
    let mlist: Value = serde_json::from_str(&mlist_raw).unwrap_or(Value::Array(vec![]));
    let existing_mkt = mlist.as_array().and_then(|a| a.iter().find(|m| m["name"] == "tcas"));
    match existing_mkt {
        Some(m) => {
            let loc = m["path"].as_str().or_else(|| m["installLocation"].as_str()).unwrap_or("");
            if same_dir(&mkt_dir, loc) {
                run_cli("claude", &["plugin", "marketplace", "update", "tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
            } else {
                run_cli("claude", &["plugin", "marketplace", "remove", "tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
                run_cli("claude", &["plugin", "marketplace", "add", &mkt_str]).map_err(|e| x.fail(code::REFUSED, &e))?;
            }
        }
        None => {
            run_cli("claude", &["plugin", "marketplace", "add", &mkt_str]).map_err(|e| x.fail(code::REFUSED, &e))?;
        }
    }

    let plist_raw = run_cli("claude", &["plugin", "list", "--json"]).map_err(|e| x.fail(code::REFUSED, &e))?;
    let plist: Value = serde_json::from_str(&plist_raw).unwrap_or(Value::Array(vec![]));
    let existing_plugin = plist.as_array().and_then(|a| a.iter().find(|p| p["id"] == "tcas@tcas"));
    let action = match existing_plugin {
        Some(_) => {
            run_cli("claude", &["plugin", "update", "tcas@tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
            "updated"
        }
        None => {
            run_cli("claude", &["plugin", "install", "tcas@tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
            "installed"
        }
    };

    let plist_after_raw = run_cli("claude", &["plugin", "list", "--json"]).map_err(|e| x.fail(code::REFUSED, &e))?;
    let plist_after: Value = serde_json::from_str(&plist_after_raw).unwrap_or(Value::Array(vec![]));
    let disabled = plist_after
        .as_array()
        .and_then(|a| a.iter().find(|p| p["id"] == "tcas@tcas"))
        .and_then(|p| p["enabled"].as_bool())
        == Some(false);
    if disabled {
        run_cli("claude", &["plugin", "enable", "tcas@tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
    }

    if x.json {
        crate::print_json(&json!({
            "harness": "claude",
            "plugin_dir": plugin_dir,
            "marketplace_dir": mkt_dir,
            "action": action,
            "enabled": true,
        }));
    } else {
        println!("{action} claude plugin tcas@tcas (marketplace {}).", x.o.tilde(&mkt_str));
        println!("Restart any running Claude Code sessions to apply.");
    }
    Ok(0)
}

fn install_agy(x: &Ctx) -> Result<i32, i32> {
    let plugin_dir = write(&x.c.home, "agy").map_err(|e| x.fail(code::INTERNAL, &e))?;
    let dir_str = plugin_dir.to_string_lossy().into_owned();
    let was_installed = agy_state(&x.c.home).plugin_installed;

    if let Err(e) = run_cli("agy", &["plugin", "install", &dir_str]) {
        if was_installed {
            let _ = run_cli("agy", &["plugin", "uninstall", "tcas"]);
            run_cli("agy", &["plugin", "install", &dir_str]).map_err(|e2| x.fail(code::REFUSED, &e2))?;
        } else {
            return Err(x.fail(code::REFUSED, &e));
        }
    }

    let st = agy_state(&x.c.home);
    if let Some(inst_dir) = &st.installed_dir {
        let bin = inst_dir.join("bin/tcas");
        if bin.is_file() && !is_executable(&bin) {
            let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
        }
    }
    if !st.plugin_enabled {
        run_cli("agy", &["plugin", "enable", "tcas"]).map_err(|e| x.fail(code::REFUSED, &e))?;
    }

    let action = if was_installed { "updated" } else { "installed" };
    if x.json {
        crate::print_json(&json!({
            "harness": "agy",
            "plugin_dir": plugin_dir,
            "installed_dir": st.installed_dir,
            "action": action,
            "enabled": true,
        }));
    } else {
        println!("{action} agy plugin tcas (from {}).", x.o.tilde(&dir_str));
        println!("Restart any running AGY sessions to apply.");
    }
    Ok(0)
}
