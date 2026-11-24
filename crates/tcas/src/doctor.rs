//! `tcas doctor` (daemon-and-cli.md section 5, plan.md section 6).
//! Never auto-starts `tcasd`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::json;
use tcas_core::config::GlobalConfig;
use tcas_core::exempt;
use tcas_core::human;
use tcas_core::proto::{
    DaemonStatus, DoctorDaemonReply, DoctorRepo, DoctorWorkspace, ExemptView, RootView, VERSION, code,
};
use tcas_core::abspath;
use tcas_sys::clock;
use tcas_sys::fsys;

use crate::Ctx;
use crate::client::Client;
use crate::plugin;

const CLAUDE_MIN_VERSION: (u64, u64, u64) = (2, 1, 295);
const CLAUDE_MIN_STR: &str = "2.1.295";
const CLAUDE_TESTED_MIN: (u64, u64, u64) = (2, 1, 327);
const CLAUDE_TESTED_MAX: (u64, u64, u64) = (2, 1, 331);
const CLAUDE_TESTED_RANGE: &str = "2.1.327 - 2.1.331";

const AGY_MIN_VERSION: (u64, u64, u64) = (1, 5, 16);
const AGY_MIN_STR: &str = "1.5.16";
const AGY_TESTED_MIN: (u64, u64, u64) = (1, 5, 16);
const AGY_TESTED_MAX: (u64, u64, u64) = (1, 5, 16);
const AGY_TESTED_RANGE: &str = "1.5.16";

#[derive(Serialize)]
struct DaemonSection {
    running: bool,
    home: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspaces: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions: Option<usize>,
    version_mismatch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct GitSection {
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    config_hooks_supported: bool,
}

#[derive(Serialize)]
struct ClaudeSection {
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    minimum_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    meets_minimum: Option<bool>,
    tested_range: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tested: Option<bool>,
    plugin_dir: String,
    plugin_files_ok: bool,
    marketplace_installed: bool,
    plugin_installed: bool,
    plugin_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_bin_executable: Option<bool>,
}

#[derive(Serialize)]
struct AgySection {
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    minimum_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    meets_minimum: Option<bool>,
    tested_range: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tested: Option<bool>,
    plugin_dir: String,
    plugin_files_ok: bool,
    plugin_installed: bool,
    plugin_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_hooks_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_bin_executable: Option<bool>,
    tcas_on_path: bool,
}

#[derive(Serialize)]
struct DoctorReport {
    cli_version: &'static str,
    protocol_version: u32,
    tcasd: DaemonSection,
    exempt: Vec<ExemptView>,
    problems: Vec<String>,
    conflicts: Vec<String>,
    workspace: Option<DoctorWorkspace>,
    git: GitSection,
    claude: ClaudeSection,
    agy: AgySection,
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let token = s.split_whitespace().next()?;
    let core = token.split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn git_info() -> GitSection {
    let out = Command::new("git").arg("--version").stdin(Stdio::null()).output();
    let (installed, version) = match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let v = s.strip_prefix("git version ").unwrap_or(&s).to_string();
            (true, Some(v))
        }
        _ => (false, None),
    };
    let git = tcasd::git::Git::new();
    let config_hooks_supported = installed && tcasd::githooks::supported(&git);
    GitSection { installed, version, config_hooks_supported }
}

fn claude_info(x: &Ctx) -> ClaudeSection {
    let out = Command::new("claude").arg("--version").stdin(Stdio::null()).output();
    let (installed, version, ver_tuple) = match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let ver = s.split_whitespace().next().unwrap_or("").to_string();
            let parsed = parse_semver(&s);
            let shown = if ver.is_empty() { s } else { ver };
            (true, Some(shown), parsed)
        }
        _ => (false, None, None),
    };
    let st = plugin::claude_state(&x.c.home);
    ClaudeSection {
        installed,
        version,
        minimum_version: CLAUDE_MIN_STR,
        meets_minimum: ver_tuple.map(|v| v >= CLAUDE_MIN_VERSION),
        tested_range: CLAUDE_TESTED_RANGE,
        tested: ver_tuple.map(|v| v >= CLAUDE_TESTED_MIN && v <= CLAUDE_TESTED_MAX),
        plugin_dir: fsys::to_str(&st.plugin_dir),
        plugin_files_ok: st.plugin_files_ok,
        marketplace_installed: st.marketplace_installed,
        plugin_installed: st.plugin_installed,
        plugin_enabled: st.plugin_enabled,
        installed_version: st.installed_version,
        cache_bin_executable: st.cache_bin_executable,
    }
}

fn tcas_on_path() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| plugin::is_executable(&d.join("tcas"))))
        .unwrap_or(false)
}

fn agy_info(x: &Ctx) -> AgySection {
    let out = Command::new("agy").arg("--version").stdin(Stdio::null()).output();
    let (installed, version, ver_tuple) = match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let ver = s.split_whitespace().next().unwrap_or("").to_string();
            let parsed = parse_semver(&s);
            let shown = if ver.is_empty() { s } else { ver };
            (true, Some(shown), parsed)
        }
        _ => (false, None, None),
    };
    let st = plugin::agy_state(&x.c.home);
    AgySection {
        installed,
        version,
        minimum_version: AGY_MIN_STR,
        meets_minimum: ver_tuple.map(|v| v >= AGY_MIN_VERSION),
        tested_range: AGY_TESTED_RANGE,
        tested: ver_tuple.map(|v| v >= AGY_TESTED_MIN && v <= AGY_TESTED_MAX),
        plugin_dir: fsys::to_str(&st.plugin_dir),
        plugin_files_ok: st.plugin_files_ok,
        plugin_installed: st.plugin_installed,
        plugin_enabled: st.plugin_enabled,
        installed_version: st.installed_version,
        installed_hooks_ok: st.installed_hooks_ok,
        installed_bin_executable: st.installed_bin_executable,
        tcas_on_path: tcas_on_path(),
    }
}

fn check_local_toml(dir: &Path, problems: &mut Vec<String>) {
    let p = dir.join(".tcas.toml");
    if let Ok(text) = std::fs::read_to_string(&p)
        && let Err(e) = tcas_core::config::parse_workspace(&text)
    {
        let msg = format!("{}: {e}", p.display());
        if !problems.contains(&msg) {
            problems.push(msg);
        }
    }
}

struct LocalState {
    exempt: Vec<ExemptView>,
    problems: Vec<String>,
    conflicts: Vec<String>,
    workspace: Option<DoctorWorkspace>,
}

fn inspect_local(x: &Ctx, cwd: &Path, git: &tcasd::git::Git) -> LocalState {
    let g = tcasd::daemon::load_global(&x.c.home);
    let mut problems = g.problems;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut exempt_views = Vec::new();
    for e in &g.exempt {
        if seen.insert((e.written.clone(), e.source.clone())) {
            exempt_views.push(ExemptView { path: e.path.clone(), written: e.written.clone(), source: e.source.clone() });
        }
    }

    let mut conflicts = Vec::new();
    let mut workspace = None;
    let wsjson: Option<tcasd::wsjson::WsJson> = std::fs::read_to_string(x.c.home.workspaces_json())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let cwd_s = fsys::to_str(cwd);

    if let Some(wj) = wsjson {
        for w in &wj.workspaces {
            for r in &w.roots {
                if let Some(e) = exempt::find(&g.exempt, &r.path) {
                    conflicts.push(format!("root {} of workspace \"{}\" is under exempt {}", r.path, w.name, e.written));
                }
                check_local_toml(Path::new(&r.path), &mut problems);
            }
            for e in &w.exempt {
                if seen.insert((e.written.clone(), e.source.clone())) {
                    exempt_views.push(ExemptView {
                        path: e.path.clone(),
                        written: e.written.clone(),
                        source: e.source.clone(),
                    });
                }
            }
            if workspace.is_none() && w.roots.iter().any(|r| abspath::contains(&r.path, &cwd_s, r.case_insensitive)) {
                let mut ws_problems = Vec::new();
                for r in &w.roots {
                    check_local_toml(Path::new(&r.path), &mut ws_problems);
                }
                let first_cfg = w
                    .roots
                    .first()
                    .and_then(|r| std::fs::read_to_string(Path::new(&r.path).join(".tcas.toml")).ok())
                    .and_then(|t| tcas_core::config::parse_workspace(&t).ok())
                    .unwrap_or_default();
                let mode = first_cfg.mode.unwrap_or_else(|| GlobalConfig::default().defaults.mode);
                let all_roots: Vec<(String, bool)> =
                    wj.workspaces.iter().flat_map(|x| x.roots.iter().map(|r| (r.path.clone(), r.case_insensitive))).collect();
                let mut ex = g.exempt.clone();
                ex.extend(w.exempt.iter().cloned());
                let mut repos = Vec::new();
                for r in &w.roots {
                    let disc = tcasd::repos::discover(git, Path::new(&r.path), r.case_insensitive, first_cfg.repos.scan_depth, &ex, &all_roots);
                    for f in disc.found {
                        let abs_path = if f.dir.is_empty() { PathBuf::from(&r.path) } else { Path::new(&r.path).join(&f.dir) };
                        let display = if w.roots.len() > 1 {
                            let p = tcas_core::relpath::join(&r.name, &f.dir);
                            format!("{p}/")
                        } else {
                            tcas_core::relpath::display(&f.dir, true)
                        };
                        repos.push(DoctorRepo {
                            display,
                            abs: fsys::to_str(&abs_path),
                            kind: f.kind,
                            branch: f.branch,
                            hooks_installed: tcasd::githooks::installed(git, &x.c.home, &abs_path),
                        });
                    }
                }
                let roots = w
                    .roots
                    .iter()
                    .map(|r| RootView {
                        name: r.name.clone(),
                        path: r.path.clone(),
                        case_insensitive: r.case_insensitive,
                        added_by: "unknown".into(),
                    })
                    .collect();
                workspace = Some(DoctorWorkspace {
                    id: w.id.clone(),
                    name: w.name.clone(),
                    mode: mode.as_str().into(),
                    roots,
                    problems: ws_problems,
                    git_hooks_config: fsys::to_str(&x.c.home.git_hooks_config()),
                    repos,
                });
            }
        }
    }

    LocalState { exempt: exempt_views, problems, conflicts, workspace }
}

fn from_daemon_status(st: DaemonStatus, proto: u32, mismatch: bool, err: Option<String>) -> DaemonSection {
    DaemonSection {
        running: true,
        home: st.home,
        pid: Some(st.pid),
        version: Some(st.version),
        protocol_version: Some(proto),
        started_at: Some(st.started_at),
        workspaces: Some(st.workspaces),
        sessions: Some(st.sessions),
        version_mismatch: mismatch,
        error: err,
    }
}

pub fn run(x: &Ctx) -> i32 {
    let cwd = std::fs::canonicalize(&x.c.cwd).unwrap_or_else(|_| x.c.cwd.clone());
    let git = git_info();
    let tcas_git = tcasd::git::Git::new();
    let claude = claude_info(x);
    let agy = agy_info(x);

    let cli_version = env!("CARGO_PKG_VERSION");
    let home_str = fsys::to_str(&x.c.home.dir);

    let (tcasd_sec, exempt, mut problems, conflicts, workspace) = match x.c.connect() {
        Ok(mut s) => {
            let sent = Client::send(&mut s, &x.c.request("doctor", json!({})));
            let recv = sent.and_then(|()| Client::receive(&s));
            match recv {
                Ok(Some(r)) if r.ok => {
                    match serde_json::from_value::<DoctorDaemonReply>(r.result.unwrap_or_default()) {
                        Ok(rep) => {
                            let mismatch = rep.daemon.version != cli_version || rep.protocol_version != VERSION;
                            let sec = from_daemon_status(rep.daemon, rep.protocol_version, mismatch, None);
                            (sec, rep.exempt, rep.problems, rep.conflicts, rep.workspace)
                        }
                        Err(e) => {
                            let loc = inspect_local(x, &cwd, &tcas_git);
                            let sec = DaemonSection {
                                running: true,
                                home: home_str.clone(),
                                pid: None,
                                version: None,
                                protocol_version: None,
                                started_at: None,
                                workspaces: None,
                                sessions: None,
                                version_mismatch: false,
                                error: Some(format!("unexpected doctor reply: {e}")),
                            };
                            (sec, loc.exempt, loc.problems, loc.conflicts, loc.workspace)
                        }
                    }
                }
                Ok(Some(r)) => {
                    let err = r.error;
                    let is_mismatch = err.as_ref().is_some_and(|e| e.code == code::VERSION_MISMATCH);
                    let msg = err.map(|e| e.message);
                    let loc = inspect_local(x, &cwd, &tcas_git);
                    let sec = DaemonSection {
                        running: true,
                        home: home_str.clone(),
                        pid: None,
                        version: None,
                        protocol_version: None,
                        started_at: None,
                        workspaces: None,
                        sessions: None,
                        version_mismatch: is_mismatch,
                        error: msg,
                    };
                    (sec, loc.exempt, loc.problems, loc.conflicts, loc.workspace)
                }
                Ok(None) | Err(_) => {
                    let loc = inspect_local(x, &cwd, &tcas_git);
                    let sec = DaemonSection {
                        running: false,
                        home: home_str.clone(),
                        pid: None,
                        version: None,
                        protocol_version: None,
                        started_at: None,
                        workspaces: None,
                        sessions: None,
                        version_mismatch: false,
                        error: None,
                    };
                    (sec, loc.exempt, loc.problems, loc.conflicts, loc.workspace)
                }
            }
        }
        Err(_) => {
            let loc = inspect_local(x, &cwd, &tcas_git);
            let sec = DaemonSection {
                running: false,
                home: home_str.clone(),
                pid: None,
                version: None,
                protocol_version: None,
                started_at: None,
                workspaces: None,
                sessions: None,
                version_mismatch: false,
                error: None,
            };
            (sec, loc.exempt, loc.problems, loc.conflicts, loc.workspace)
        }
    };

    if workspace.is_none() {
        check_local_toml(&cwd, &mut problems);
        if let Some(top) = tcas_git.toplevel(&cwd)
            && top != cwd
        {
            check_local_toml(&top, &mut problems);
        }
    }

    let report = DoctorReport {
        cli_version,
        protocol_version: VERSION,
        tcasd: tcasd_sec,
        exempt,
        problems,
        conflicts,
        workspace,
        git,
        claude,
        agy,
    };

    if x.json {
        crate::print_json(&serde_json::to_value(&report).unwrap());
    } else {
        print_human(x, &report);
    }
    0
}

fn print_human(x: &Ctx, r: &DoctorReport) {
    println!("tcas       {} (protocol {})", r.cli_version, r.protocol_version);
    if r.tcasd.running {
        let ver = r.tcasd.version.as_deref().unwrap_or("?");
        let proto = r.tcasd.protocol_version.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
        let pid = r.tcasd.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
        let up = r
            .tcasd
            .started_at
            .map(|t| human::duration(clock::now_ms() - t))
            .unwrap_or_else(|| "?".into());
        println!("tcasd      running, pid {pid}, version {ver} (protocol {proto}), up {up}");
        println!("           home {}", x.o.tilde(&r.tcasd.home));
        if r.tcasd.version_mismatch {
            println!("           warning: version mismatch with tcas {}; restart tcasd (tcas daemon stop)", r.cli_version);
        }
        if let Some(e) = &r.tcasd.error {
            println!("           note: {e}");
        }
    } else {
        println!("tcasd      not running (home {})", x.o.tilde(&r.tcasd.home));
    }

    println!();
    if let Some(ws) = &r.workspace {
        println!("workspace  \"{}\" ({})  mode {}", ws.name, ws.id, ws.mode);
        for root in &ws.roots {
            println!("  root     {:<14} {}", root.name, x.o.tilde(&root.path));
        }
        if ws.repos.is_empty() {
            println!("  repos    none");
        } else {
            for repo in &ws.repos {
                let hook_st = if repo.hooks_installed { "hooks installed" } else { "hooks NOT installed" };
                let br = repo.branch.as_deref().unwrap_or("");
                println!("  repo     {:<18} {:<10} {:<16} {}", repo.display, repo.kind, br, hook_st);
            }
        }
    } else {
        println!("workspace  cwd is not in any workspace ({})", x.o.tilde(&x.c.cwd.to_string_lossy()));
    }

    println!();
    if r.git.installed {
        let sup = if r.git.config_hooks_supported { "supported" } else { "NOT supported" };
        println!("git        {}   config hooks: {sup}", r.git.version.as_deref().unwrap_or("?"));
    } else {
        println!("git        not found on PATH");
    }

    println!();
    if r.claude.installed {
        let min_s = match r.claude.meets_minimum {
            Some(true) => "ok",
            Some(false) => "TOO OLD",
            None => "unknown",
        };
        let test_s = match r.claude.tested {
            Some(true) => "yes",
            Some(false) => "untested",
            None => "unknown",
        };
        println!(
            "claude     {} (minimum {}: {min_s}; tested {}: {test_s})",
            r.claude.version.as_deref().unwrap_or("?"),
            r.claude.minimum_version,
            r.claude.tested_range
        );
    } else {
        println!("claude     not found on PATH");
    }
    println!(
        "  plugin files ({}): {}",
        x.o.tilde(&r.claude.plugin_dir),
        if r.claude.plugin_files_ok { "ok" } else { "missing or incomplete" }
    );
    let inst = match (r.claude.plugin_installed, r.claude.plugin_enabled) {
        (true, true) => {
            let v = r.claude.installed_version.as_deref().unwrap_or("?");
            format!("installed ({v}), enabled")
        }
        (true, false) => {
            let v = r.claude.installed_version.as_deref().unwrap_or("?");
            format!("installed ({v}), DISABLED")
        }
        (false, _) => "not installed (run: tcas plugin install claude)".into(),
    };
    println!("  installed (tcas@tcas): {inst}");
    if let Some(exec) = r.claude.cache_bin_executable
        && !exec
    {
        println!("  warning: cached bin/tcas is not executable; reinstall with: tcas plugin install claude");
    }

    println!();
    if r.agy.installed {
        let min_s = match r.agy.meets_minimum {
            Some(true) => "ok",
            Some(false) => "TOO OLD",
            None => "unknown",
        };
        let test_s = match r.agy.tested {
            Some(true) => "yes",
            Some(false) => "untested",
            None => "unknown",
        };
        println!(
            "agy        {} (minimum {}: {min_s}; tested {}: {test_s})",
            r.agy.version.as_deref().unwrap_or("?"),
            r.agy.minimum_version,
            r.agy.tested_range
        );
    } else {
        println!("agy        not found on PATH");
    }
    println!(
        "  plugin files ({}): {}",
        x.o.tilde(&r.agy.plugin_dir),
        if r.agy.plugin_files_ok { "ok" } else { "missing or incomplete" }
    );
    let agy_inst = match (r.agy.plugin_installed, r.agy.plugin_enabled) {
        (true, true) => {
            let v = r.agy.installed_version.as_deref().unwrap_or("?");
            format!("installed ({v}), enabled")
        }
        (true, false) => {
            let v = r.agy.installed_version.as_deref().unwrap_or("?");
            format!("installed ({v}), DISABLED")
        }
        (false, _) => "not installed (run: tcas plugin install agy)".into(),
    };
    println!("  installed (tcas): {agy_inst}");
    if r.agy.installed_hooks_ok == Some(false) {
        println!("  warning: installed plugin is missing root hooks.json; reinstall with: tcas plugin install agy");
    }
    if r.agy.installed_bin_executable == Some(false) {
        println!("  warning: installed bin/tcas is not executable; reinstall with: tcas plugin install agy");
    }
    if r.agy.installed && !r.agy.tcas_on_path {
        println!("  note: tcas is not on PATH (AGY run_command does not add plugin bin/ to PATH)");
    }

    println!();
    println!("exempt directories:");
    let w = r.exempt.iter().map(|e| e.written.chars().count() + 2).max().unwrap_or(0).max(20);
    for e in &r.exempt {
        let pad = " ".repeat(w.saturating_sub(e.written.chars().count()));
        println!("  {}{pad}{}", e.written, e.source);
    }

    if !r.problems.is_empty() {
        println!();
        println!("problems:");
        for p in &r.problems {
            println!("  {p}");
        }
    }
    if !r.conflicts.is_empty() {
        println!();
        println!("conflicts:");
        for c in &r.conflicts {
            println!("  {c}");
        }
    }
}
