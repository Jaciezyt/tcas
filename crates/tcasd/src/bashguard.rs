//! The command guard (harness.md section 5.1): which recognised git commands
//! a session may not run while other sessions hold leases in the checkout
//! the command acts on. The command is parsed in `tcas-core`; this decides
//! with the lease table.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::json;
use tcas_core::guard::{self, Action, GitCall};
use tcas_core::shell::{self, Parsed};
use tcas_core::{abspath, brief, lease, relpath};
use tcas_core::human::short_session;
use tcas_sys::fsys;

use crate::daemon::R;
use crate::engine::{self, Env};
use crate::store::{self, LeaseRow};
use crate::text::Viewer;
use crate::world::Ws;

/// A refused command: the reason for the model and the rule for the event.
pub struct Refusal {
    pub rule: &'static str,
    pub reason: String,
    pub ws: Option<String>,
    pub repo: Option<String>,
}

/// The checkout a git call acts on.
struct Checkout<'a> {
    ws: &'a Ws,
    root: String,
    /// Relative to the root.
    dir: String,
    abs: PathBuf,
}

impl Checkout<'_> {
    fn shown(&self) -> String {
        if self.dir.is_empty() && !self.ws.multi_root() {
            ".".into()
        } else {
            self.ws.display(&self.root, &self.dir, true)
        }
    }
}

/// The checkout containing `dir`, among the workspace's known ones.
fn checkout_of<'a>(env: &'a Env, dir: &str) -> Option<Checkout<'a>> {
    let (ws, root) = env.world.root_of(dir)?;
    let rel = abspath::relative(&root.path, dir);
    let key = relpath::key(&rel, root.ci);
    let best = ws
        .repos
        .iter()
        .filter(|r| r.root == root.name && relpath::contains(&relpath::key(&r.dir, root.ci), &key))
        .max_by_key(|r| r.dir.len())?;
    Some(Checkout { ws, root: root.name.clone(), dir: best.dir.clone(), abs: ws.repo_abs(&root.name, &best.dir) })
}

/// Leases of other sessions in this checkout (not in checkouts nested in it).
fn others_in(env: &Env, co: &Checkout, me: &str) -> R<Vec<LeaseRow>> {
    let ci = co.ws.ci(&co.root);
    let mine_key = relpath::key(&co.dir, ci);
    let all = store::leases_ws(env.c, &co.ws.row.id)?;
    Ok(all
        .into_iter()
        .filter(|l| l.session != me && l.root == co.root)
        .filter(|l| match &l.repo {
            Some(r) => relpath::key(r, ci) == mine_key,
            // A directory lease spanning checkouts counts in each of them.
            None => l.is_prefix && (relpath::contains(&l.path_key, &mine_key) || relpath::contains(&mine_key, &l.path_key)),
        })
        .collect())
}

fn holders(ls: &[LeaseRow]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for l in ls {
        if seen.insert(l.session.clone()) {
            out.push(short_session(&l.session));
        }
    }
    out.join(", ")
}

fn refuse(rule: &'static str, co: &Checkout, others: &[LeaseRow], what: &str, instead: &str) -> Refusal {
    let shown = co.shown();
    let mut reason = format!(
        "tcas: {what} is refused in {shown}: other sessions hold leases there ({}).",
        holders(others)
    );
    if !instead.is_empty() {
        reason.push('\n');
        reason.push_str(instead);
    }
    Refusal { rule, reason, ws: Some(co.ws.row.id.clone()), repo: Some(shown) }
}

/// Paths under `dir` (absolute) that `rels` name, as root-relative keys.
fn pathspec_keys(co: &Checkout, base: &str, words: &[String]) -> Vec<(String, bool)> {
    let ci = co.ws.ci(&co.root);
    let root_path = co.ws.root(&co.root).map(|r| r.path.clone()).unwrap_or_default();
    words
        .iter()
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.strip_prefix(":(top)").or_else(|| w.strip_prefix(":/")).map(|r| (r, true)).unwrap_or((w, false));
            let abs = if w.1 { shell::join_dir(&fsys::to_str(&co.abs), w.0) } else { shell::join_dir(base, w.0) };
            let is_dir = w.0.ends_with('/') || w.0 == "." || Path::new(&abs).is_dir();
            (relpath::key(&abspath::relative(&root_path, &abs), ci), is_dir)
        })
        .collect()
}

/// Every lease in `ls` overlapping one of the path keys.
fn overlapping<'a>(ls: &'a [LeaseRow], keys: &[(String, bool)]) -> Vec<&'a LeaseRow> {
    ls.iter()
        .filter(|l| {
            keys.iter().any(|(k, is_dir)| {
                lease::covers(&l.path_key, l.is_prefix, k, *is_dir) || *is_dir && relpath::contains(k, &l.path_key)
            })
        })
        .collect()
}

fn path_list(env: &Env, co: &Checkout, ls: &[&LeaseRow]) -> String {
    let v = Viewer::new(&env.cwd, env.now);
    let mut out: Vec<String> = Vec::new();
    for l in ls.iter().take(8) {
        let b = env
            .c
            .query_row("SELECT brief FROM session WHERE id=?1", [&l.session], |r| r.get::<_, Option<String>>(0))
            .ok()
            .flatten();
        let b = l.brief.clone().or(b).map(|t| format!(" {}", brief::quoted(&t))).unwrap_or_default();
        out.push(format!(
            "  {}    leased by {}{b}",
            v.path(&co.ws.abs(&l.root, &l.path), l.is_prefix),
            short_session(&l.session)
        ));
    }
    if ls.len() > 8 {
        out.push(format!("  … and {} more", ls.len() - 8));
    }
    out.join("\n")
}

/// The guard for one Bash command (harness.md section 5.1). `None` lets it run.
pub fn check(env: &Env, sid: &str, command: &str, cwd: &str) -> R<Option<Refusal>> {
    let home = tcas_sys::home::user_home().map(|h| fsys::to_str(&h));
    let parsed = shell::parse(command, cwd, home.as_deref());
    if let Some(r) = protected(env, &parsed) {
        return Ok(Some(r));
    }
    let on = |dir: &str| checkout_of(env, dir).is_none_or(|c| c.ws.cfg.bash.git_guard);
    for s in &parsed.commands {
        if guard::sets_tcas_allow(s) {
            let co = checkout_of(env, &s.dir);
            return Ok(Some(Refusal {
                rule: "tcas_allow",
                reason: "tcas: TCAS_ALLOW is for the user, to let one of their own git commands through. A session \
                         may not set it. Remove it; if the user should make this change, tell them."
                    .into(),
                ws: co.as_ref().map(|c| c.ws.row.id.clone()),
                repo: co.as_ref().map(Checkout::shown),
            }));
        }
        let Some(call) = guard::git_call(s) else { continue };
        if !on(&call.dir) {
            continue;
        }
        if let Some(r) = check_git(env, sid, &call)? {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

/// Arguments or redirect targets naming tcas's home or a workspace's
/// `.tcas.toml` (harness.md section 5.1): refused whoever else is there.
fn protected(env: &Env, p: &Parsed) -> Option<Refusal> {
    let home = &env.d.home.dir;
    let homes: Vec<String> = [fsys::to_str(home), fsys::to_str(&std::fs::canonicalize(home).unwrap_or_else(|_| home.clone()))]
        .into_iter()
        .collect();
    for s in &p.commands {
        let words: Vec<&str> = if guard::is_read_only(s) { vec![] } else { s.words.iter().skip(1).map(|w| w.text()).collect() };
        for w in words.into_iter().chain(s.redirects.iter().map(|w| w.text())) {
            let w = w.split_once('=').map(|(_, v)| v).filter(|_| w.starts_with('-')).unwrap_or(w);
            if w.is_empty() || w.starts_with('-') && !w.contains('/') {
                continue;
            }
            let abs = shell::join_dir(&s.dir, w);
            if homes.iter().any(|h| abspath::contains(h, &abs, false)) {
                let v = Viewer::new(&env.cwd, env.now);
                return Some(Refusal {
                    rule: "config",
                    reason: crate::text::config_file(&v.tilde(&abs), "part of tcas's own configuration and data"),
                    ws: None,
                    repo: None,
                });
            }
            if let Some((ws, root)) = env.world.root_of(&abs) {
                let name = shell::basename(&abs);
                let is = if root.ci { name.eq_ignore_ascii_case(".tcas.toml") } else { name == ".tcas.toml" };
                if is {
                    let v = Viewer::new(&env.cwd, env.now);
                    let co = Path::new(&abs).parent().and_then(|d| checkout_of(env, &fsys::to_str(d)));
                    return Some(Refusal {
                        rule: "config",
                        reason: crate::text::config_file(&v.path(&abs, false), "tcas's configuration for this workspace"),
                        ws: Some(ws.row.id.clone()),
                        repo: co.as_ref().map(Checkout::shown),
                    });
                }
            }
        }
    }
    None
}

const TELL_USER: &str = "If it needs doing, tell the user.";

fn check_git(env: &Env, sid: &str, call: &GitCall) -> R<Option<Refusal>> {
    // The checkout it acts on. `--git-dir`/`--work-tree` written out name it;
    // a computed one is a dynamic command.
    let dir = call.work_tree.as_deref().map(|w| shell::join_dir(&call.dir, w)).unwrap_or_else(|| call.dir.clone());
    let Some(co) = checkout_of(env, &dir) else { return Ok(None) };
    let others = others_in(env, &co, sid)?;
    if others.is_empty() {
        // Worktree removal is judged by its target, which may be elsewhere.
        if let Action::WorktreeRemove(t) | Action::WorktreeMove(t) = guard::classify(call) {
            return worktree_target(env, sid, &co, &t, call);
        }
        return Ok(None);
    }
    let mut call = call.clone();
    for _ in 0..8 {
        let action = if !call.dir_known { Action::Dynamic } else { guard::classify(&call) };
        let r = match action {
            Action::Allow => None,
            Action::AddAll => Some(refuse(
                "add_all",
                &co,
                &others,
                "git add -A / --all / -u / .",
                "Add only the files you changed:  git add <path>…   or commit them with:  tcas commit -m \"…\"",
            )),
            Action::AddPaths(ps) => {
                let keys = pathspec_keys(&co, &call.dir, &ps);
                let dirs: Vec<(String, bool)> = keys.into_iter().filter(|k| k.1).collect();
                let files: Vec<LeaseRow> = others.iter().filter(|l| !l.is_prefix).cloned().collect();
                let hit: Vec<&LeaseRow> = overlapping(&files, &dirs);
                let dirty = engine::dirty_of(env, co.ws, &hit);
                let hit: Vec<&LeaseRow> =
                    hit.into_iter().filter(|l| dirty.contains(&(l.root.clone(), l.path_key.clone()))).collect();
                (!hit.is_empty()).then(|| {
                    let shown = co.shown();
                    Refusal {
                        rule: "add_dir",
                        reason: format!(
                            "tcas: git add of a directory is refused in {shown}: it holds files other sessions are changing:\n{}\n\
                             Add only the files you changed:  git add <path>…   or commit them with:  tcas commit -m \"…\"",
                            path_list(env, &co, &hit)
                        ),
                        ws: Some(co.ws.row.id.clone()),
                        repo: Some(shown),
                    }
                })
            }
            Action::CommitAll => Some(refuse(
                "commit_all",
                &co,
                &others,
                "git commit -a / --all",
                "It would commit their uncommitted work too. Commit only your own files:  tcas commit -m \"…\"",
            )),
            Action::CommitNoVerify => Some(refuse(
                "no_verify",
                &co,
                &others,
                "git commit --no-verify / -n",
                "It skips the check that keeps other sessions' files out of your commit. Remove it, or use:  tcas commit -m \"…\"",
            )),
            Action::CommitStaged { .. } => {
                let mine = store::leases_session(env.c, sid)?;
                let ci = co.ws.ci(&co.root);
                let staged = env.d.git.staged(&co.abs);
                let foreign: Vec<String> = staged
                    .iter()
                    .filter(|p| {
                        let k = relpath::key(&relpath::join(&co.dir, p), ci);
                        !mine.iter().any(|l| l.workspace == co.ws.row.id && l.root == co.root && lease::covers(&l.path_key, l.is_prefix, &k, false))
                    })
                    .cloned()
                    .collect();
                (!foreign.is_empty()).then(|| {
                    let shown = co.shown();
                    Refusal {
                        rule: "commit_staged",
                        reason: format!(
                            "tcas: git commit without paths is refused in {shown}: the index holds files that are not under \
                             this session's leases ({}), and other sessions are working there ({}). Commit only your own \
                             files:  tcas commit -m \"…\"",
                            foreign.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
                            holders(&others)
                        ),
                        ws: Some(co.ws.row.id.clone()),
                        repo: Some(shown),
                    }
                })
            }
            Action::CommitPaths(_) => None,
            Action::StashSave => Some(refuse(
                "stash",
                &co,
                &others,
                "git stash",
                "It would take their uncommitted changes out of the working tree too. Leave the changes where they are.",
            )),
            Action::ResetHard => Some(refuse(
                "reset_hard",
                &co,
                &others,
                "git reset --hard / --merge / --keep",
                "It would discard their uncommitted changes. To undo your own changes:  git restore <path>…",
            )),
            Action::RestorePaths(ps) => {
                let keys = pathspec_keys(&co, &call.dir, &ps);
                let mine = store::leases_session(env.c, sid)?;
                let outside_mine = keys.iter().any(|(k, is_dir)| {
                    !mine.iter().any(|l| l.workspace == co.ws.row.id && l.root == co.root && lease::covers(&l.path_key, l.is_prefix, k, *is_dir))
                });
                let hit = overlapping(&others, &keys);
                outside_mine.then(|| {
                    let shown = co.shown();
                    Refusal {
                        rule: "restore",
                        reason: format!(
                            "tcas: restoring these paths is refused in {shown}: they cover files that are not under this \
                             session's leases{}.\nRestore only paths you hold:  git restore <path>…",
                            if hit.is_empty() { String::new() } else { format!(":\n{}", path_list(env, &co, &hit)) }
                        ),
                        ws: Some(co.ws.row.id.clone()),
                        repo: Some(shown),
                    }
                })
            }
            Action::Discard => Some(refuse("discard", &co, &others, "git checkout -f / switch --discard-changes", TELL_USER)),
            Action::Clean => Some(refuse(
                "clean",
                &co,
                &others,
                "git clean",
                "It would delete files other sessions created. Delete your own new files by name.",
            )),
            Action::Switch => Some(refuse(
                "switch",
                &co,
                &others,
                "Switching branches",
                &format!("It changes the files they are working on. {TELL_USER}"),
            )),
            Action::CheckoutName(n) => {
                if env.d.git.is_commit(&co.abs, &n) {
                    Some(refuse(
                        "switch",
                        &co,
                        &others,
                        "Switching branches",
                        &format!("It changes the files they are working on. {TELL_USER}"),
                    ))
                } else {
                    call.args = vec![tcas_core::shell::Word::Lit("--".into()), tcas_core::shell::Word::Lit(n)];
                    continue;
                }
            }
            Action::Rewrite(sub) => Some(refuse(
                "rewrite",
                &co,
                &others,
                &format!("git {sub}"),
                &format!("It rewrites files they are working on. {TELL_USER}"),
            )),
            Action::WorktreeRemove(t) | Action::WorktreeMove(t) => return worktree_target(env, sid, &co, &t, &call),
            Action::Submodule(k) => Some(refuse(
                "submodule",
                &co,
                &others,
                &format!("git submodule {k}"),
                &format!("It rewrites files in the submodules. {TELL_USER}"),
            )),
            Action::Dynamic => Some(refuse(
                "dynamic",
                &co,
                &others,
                "A git command whose subcommand, paths or directory are computed",
                "Write it out explicitly (no variables, $(…), xargs or eval), so it can be checked.",
            )),
            Action::Unknown(name) => {
                let def = guard::alias_in_config(&call, &name)
                    .map(str::to_string)
                    .or_else(|| env.d.git.config_get(&co.abs, &format!("alias.{name}")));
                match def {
                    None => None,
                    Some(d) => match guard::expand_alias(&call, &d) {
                        Some(next) => {
                            call = next;
                            continue;
                        }
                        None => Some(refuse(
                            "dynamic",
                            &co,
                            &others,
                            &format!("The git alias {name} (a shell command)"),
                            "Write out the git commands it runs, so they can be checked.",
                        )),
                    },
                }
            }
        };
        return Ok(r);
    }
    Ok(Some(refuse("dynamic", &co, &others, "This git alias", "Write out the git command it stands for.")))
}

/// `git worktree remove/move <dir>`: refused when the target checkout holds
/// other sessions' leases, wherever the command runs from.
fn worktree_target(env: &Env, sid: &str, from: &Checkout, target: &str, call: &GitCall) -> R<Option<Refusal>> {
    let abs = shell::join_dir(&call.dir, target);
    let abs = std::fs::canonicalize(&abs).map(|p| fsys::to_str(&p)).unwrap_or(abs);
    let Some(co) = checkout_of(env, &abs) else { return Ok(None) };
    let ci = co.ws.ci(&co.root);
    let target = fsys::to_str(&co.abs);
    if !(abspath::contains(&target, &abs, ci) && abspath::contains(&abs, &target, ci)) || co.abs == from.abs {
        return Ok(None);
    }
    let others = others_in(env, &co, sid)?;
    Ok((!others.is_empty()).then(|| {
        refuse(
            "worktree",
            &co,
            &others,
            "Removing or moving this worktree",
            &format!("Other sessions are working in it. {TELL_USER}"),
        )
    }))
}

/// Records a refusal.
pub fn record(env: &Env, sid: &str, r: &Refusal, command: &str) -> R<()> {
    let ws = r.ws.clone().or_else(|| env.world.by_cwd(&env.cwd).map(|w| w.row.id.clone()));
    let one: String = command.chars().take(500).collect();
    let mut payload = json!({ "rule": r.rule, "command": one });
    if let Some(repo) = &r.repo {
        payload["repo"] = json!(repo);
    }
    env.event(ws.as_deref(), Some(sid), "bash.blocked", payload)
}
