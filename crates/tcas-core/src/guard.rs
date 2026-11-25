//! Command guard: classify the git invocations found by [`crate::shell`]
//! (harness.md sections 5.1 and 5.5). The rules here only say what a command *is*;
//! whether it is refused depends on leases and is decided by the caller.

use crate::shell::{self, Parsed, Simple, Word};

/// A parsed `git` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCall {
    /// Directory git runs in after applying every `-C` (a relative `-C` joins `dir`).
    pub dir: String,
    /// False when the directory depends on something computed.
    pub dir_known: bool,
    /// `--git-dir` as written.
    pub git_dir: Option<String>,
    /// `--work-tree` as written.
    pub work_tree: Option<String>,
    /// `-c key=value` pairs in order.
    pub config: Vec<(String, String)>,
    /// The subcommand.
    pub sub: Word,
    /// Everything after the subcommand.
    pub args: Vec<Word>,
    /// Environment the command runs with.
    pub env: Vec<(String, Word)>,
}

impl GitCall {
    /// The working tree the call acts on, as written (harness.md section 5.1):
    /// `--work-tree`, else the checkout whose `.git` `--git-dir` names (or the
    /// git directory itself), else the directory it runs in.
    pub fn work_dir(&self) -> String {
        if let Some(w) = &self.work_tree {
            return shell::join_dir(&self.dir, w);
        }
        if let Some(g) = &self.git_dir {
            let gd = shell::join_dir(&self.dir, g);
            if shell::basename(&gd) == ".git"
                && let Some((parent, _)) = gd.rsplit_once('/')
            {
                return if parent.is_empty() { "/".to_string() } else { parent.to_string() };
            }
            return gd;
        }
        self.dir.clone()
    }
}

/// What a git invocation does, as far as the guard rules care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Read-only or otherwise not restricted.
    Allow,
    /// `add -A/--all/-u/--update`, `add .`, `add :/`.
    AddAll,
    /// `add <paths>`; a directory or `.`-like path is for the caller to check.
    AddPaths(Vec<String>),
    /// `commit -a/--all/-am`.
    CommitAll,
    /// `commit -n/--no-verify` (reported over [`Action::CommitAll`] when both).
    CommitNoVerify,
    /// `commit` with no pathspec.
    CommitStaged { amend: bool },
    /// `commit -- <paths>` or `commit <paths>`.
    CommitPaths(Vec<String>),
    /// Any form of `stash` that creates a stash.
    StashSave,
    /// `reset --hard/--merge/--keep`.
    ResetHard,
    /// `checkout -- <paths>`, `checkout .`, `restore <paths>` (not `restore --staged` alone).
    RestorePaths(Vec<String>),
    /// `checkout -f/--force`, `switch -f/--discard-changes`.
    Discard,
    /// `clean` without `-n/--dry-run`.
    Clean,
    /// `switch`, `checkout -b/-B/--detach/--orphan`.
    Switch,
    /// `checkout <name>`: a branch or a path, the caller decides.
    CheckoutName(String),
    /// `checkout <first> <paths…>`: `first` is a tree-ish when it names a
    /// commit, otherwise one more path. The caller decides.
    CheckoutPaths { first: String, paths: Vec<String> },
    /// `rebase merge pull cherry-pick revert am`, `apply` without `--check`; the subcommand.
    Rewrite(String),
    /// `worktree remove <dir>`; the dir as written.
    WorktreeRemove(String),
    /// `worktree move <dir> …`; the source dir as written.
    WorktreeMove(String),
    /// `submodule update/deinit`.
    Submodule(String),
    /// The subcommand or a key argument is not literal.
    Dynamic,
    /// Not a built-in subcommand: possibly an alias.
    Unknown(String),
}

/// Subcommands that exist in git and need no rule of their own.
const KNOWN: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-ref-format",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "describe",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "difftool",
    "fast-export",
    "fast-import",
    "fetch",
    "filter-branch",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "gc",
    "get-tar-commit-id",
    "grep",
    "gui",
    "hash-object",
    "help",
    "init",
    "interpret-trailers",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-tree",
    "mergetool",
    "mktree",
    "mv",
    "name-rev",
    "notes",
    "pack-objects",
    "pack-refs",
    "prune",
    "pull",
    "push",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "remote",
    "repack",
    "replace",
    "request-pull",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "send-email",
    "shortlog",
    "show",
    "show-branch",
    "show-ref",
    "sparse-checkout",
    "stash",
    "status",
    "submodule",
    "switch",
    "symbolic-ref",
    "tag",
    "unpack-file",
    "unpack-objects",
    "update-index",
    "update-ref",
    "update-server-info",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
    "stage",
    "diagnose",
    "scalar",
    "bugreport",
    "citool",
    "gitk",
    "mailinfo",
    "mailsplit",
    "patch-id",
    "stripspace",
    "show-index",
    "multi-pack-index",
    "backfill",
    "survey",
];

/// The `git` invocation in `s`, if its program basename is `git`.
pub fn git_call(s: &Simple) -> Option<GitCall> {
    let head = s.words.first()?.lit()?;
    if shell::basename(head) != "git" {
        return None;
    }
    let mut c = GitCall {
        dir: s.dir.clone(),
        dir_known: s.dir_known,
        git_dir: None,
        work_tree: None,
        config: Vec::new(),
        sub: Word::Lit(String::new()),
        args: Vec::new(),
        env: s.env.clone(),
    };
    let w = &s.words;
    let mut i = 1;
    while i < w.len() {
        let Some(t) = w[i].lit() else {
            // A computed option such as `--git-dir=$G`: the repository is not known.
            if w[i].text().starts_with('-') {
                c.dir_known = false;
                i += 1;
                continue;
            }
            break;
        };
        if !t.starts_with('-') || t == "-" {
            break;
        }
        match t {
            "-C" => {
                match w.get(i + 1)? {
                    Word::Lit(v) => {
                        if !v.is_empty() {
                            c.dir = shell::join_dir(&c.dir, v);
                            if v.starts_with('/') {
                                c.dir_known = true;
                            }
                        }
                    }
                    Word::Dynamic(_) => c.dir_known = false,
                }
                i += 2;
            }
            "-c" => {
                match w.get(i + 1)? {
                    Word::Lit(kv) => {
                        let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
                        c.config.push((k.to_string(), v.to_string()));
                    }
                    // `-c alias.st=$X st`: what the alias does is computed.
                    Word::Dynamic(raw) => {
                        if raw.to_ascii_lowercase().starts_with("alias.") {
                            c.dir_known = false;
                        }
                    }
                }
                i += 2;
            }
            "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" | "--config-env" => {
                let v = w.get(i + 1)?;
                set_path_opt(&mut c, t, v);
                i += 2;
            }
            _ => {
                if let Some((k, v)) = t.split_once('=') {
                    set_path_opt(&mut c, k, &Word::Lit(v.to_string()));
                    if k == "--config-env"
                        && let Some((name, _)) = v.split_once('=')
                    {
                        c.config.push((name.to_string(), String::new()));
                    }
                }
                i += 1;
            }
        }
    }
    c.sub = w.get(i)?.clone();
    c.args = w[i + 1..].to_vec();
    Some(c)
}

fn set_path_opt(c: &mut GitCall, name: &str, v: &Word) {
    let slot = match name {
        "--git-dir" => &mut c.git_dir,
        "--work-tree" => &mut c.work_tree,
        _ => return,
    };
    match v {
        Word::Lit(s) => *slot = Some(s.clone()),
        Word::Dynamic(raw) => {
            *slot = Some(raw.clone());
            c.dir_known = false;
        }
    }
}

/// Options and positionals of a subcommand's arguments.
#[derive(Default)]
struct Scan {
    /// `--name` for long options and `-x` for each short letter.
    flags: Vec<String>,
    /// Positionals before any `--`.
    pos: Vec<Word>,
    /// Words after `--`.
    after: Vec<Word>,
    dd: bool,
}

impl Scan {
    fn has(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|f| names.contains(&f.as_str()))
    }

    /// Every positional, before and after `--`.
    fn all(&self) -> impl Iterator<Item = &Word> {
        self.pos.iter().chain(self.after.iter())
    }

    fn any_dynamic(&self) -> bool {
        self.all().any(Word::is_dynamic)
    }
}

/// `short_val`: short letters that take a value (attached or the next word).
/// `short_opt`: short letters whose value is attached only (rest of the cluster).
/// `long_val`: long options that take a value in the next word when written without `=`.
fn scan(args: &[Word], short_val: &str, short_opt: &str, long_val: &[&str]) -> Scan {
    let mut r = Scan::default();
    let mut i = 0;
    while i < args.len() {
        let w = &args[i];
        i += 1;
        if r.dd {
            r.after.push(w.clone());
            continue;
        }
        let Some(s) = w.lit() else {
            // An option whose value is computed (`--message="$MSG"`, `-m"$MSG"`)
            // is still that option; anything else computed is a positional.
            let t = w.text();
            if let Some((name, _)) = t.strip_prefix("--").and_then(|l| l.split_once('='))
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                r.flags.push(format!("--{name}"));
            } else if let Some(ch) = t.strip_prefix('-').and_then(|x| x.chars().next())
                && (short_val.contains(ch) || short_opt.contains(ch))
            {
                r.flags.push(format!("-{ch}"));
            } else {
                r.pos.push(w.clone());
            }
            continue;
        };
        if s == "--" {
            r.dd = true;
        } else if let Some(l) = s.strip_prefix("--") {
            let (name, has_val) = match l.split_once('=') {
                Some((n, _)) => (n, true),
                None => (l, false),
            };
            r.flags.push(format!("--{name}"));
            if !has_val && long_val.contains(&name) {
                i += 1;
            }
        } else if s.len() > 1 && s.starts_with('-') {
            let cl: Vec<char> = s[1..].chars().collect();
            for (k, ch) in cl.iter().enumerate() {
                r.flags.push(format!("-{ch}"));
                if short_val.contains(*ch) {
                    if k + 1 == cl.len() {
                        i += 1;
                    }
                    break;
                }
                if short_opt.contains(*ch) {
                    break;
                }
            }
        } else {
            r.pos.push(w.clone());
        }
    }
    r
}

fn texts<'a>(it: impl Iterator<Item = &'a Word>) -> Vec<String> {
    it.map(|w| w.text().to_string()).collect()
}

/// A pathspec naming the whole working tree, or all of the current directory:
/// `.`, `./`, `:/`, `:(top)`, `:/.`, `:(top)./`.
fn whole_tree(w: &Word) -> bool {
    let Some(p) = w.lit() else { return false };
    let rest = p.strip_prefix(":/").or_else(|| p.strip_prefix(":(top)")).unwrap_or(p);
    !p.is_empty() && matches!(rest, "" | "." | "./")
}

/// Classify a git invocation.
pub fn classify(c: &GitCall) -> Action {
    let sub = match &c.sub {
        Word::Dynamic(_) => return Action::Dynamic,
        Word::Lit(s) => s.as_str(),
    };
    let a = &c.args[..];
    match sub {
        "add" | "stage" => {
            let s = scan(a, "", "", &["pathspec-from-file", "chmod"]);
            // `add -p`/`-i` with no paths goes through the whole tree.
            let whole_patch = s.all().next().is_none() && s.has(&["-p", "--patch", "-i", "--interactive"]);
            if s.has(&["-A", "--all", "-u", "--update"]) || s.all().any(whole_tree) || whole_patch {
                Action::AddAll
            } else if s.any_dynamic() || s.has(&["--pathspec-from-file"]) {
                Action::Dynamic
            } else {
                Action::AddPaths(texts(s.all()))
            }
        }
        "commit" => {
            let s = scan(
                a,
                "mFCct",
                "Su",
                &[
                    "message",
                    "file",
                    "reuse-message",
                    "reedit-message",
                    "fixup",
                    "squash",
                    "author",
                    "date",
                    "template",
                    "cleanup",
                    "trailer",
                    "pathspec-from-file",
                ],
            );
            if s.has(&["-n", "--no-verify"]) {
                Action::CommitNoVerify
            } else if s.has(&["-a", "--all"]) {
                Action::CommitAll
            } else if s.any_dynamic() || s.has(&["--pathspec-from-file"]) {
                Action::Dynamic
            } else if s.all().next().is_some() && !s.has(&["-i", "--include"]) {
                Action::CommitPaths(texts(s.all()))
            } else {
                // `--include` commits what is staged as well as the paths.
                Action::CommitStaged {
                    amend: s.has(&["--amend"]),
                }
            }
        }
        "stash" => match a.first() {
            None => Action::StashSave,
            Some(Word::Dynamic(_)) => Action::Dynamic,
            Some(Word::Lit(s)) => match s.as_str() {
                "list" | "show" | "pop" | "apply" | "drop" | "clear" | "create" | "store" => {
                    Action::Allow
                }
                "branch" => Action::Switch,
                _ => Action::StashSave,
            },
        },
        "reset" => {
            let s = scan(a, "", "", &[]);
            if s.has(&["--hard", "--merge", "--keep"]) {
                Action::ResetHard
            } else {
                Action::Allow
            }
        }
        "restore" => {
            let s = scan(a, "s", "", &["source", "pathspec-from-file", "conflict"]);
            let staged_only = s.has(&["--staged", "-S"]) && !s.has(&["--worktree", "-W"]);
            if staged_only {
                Action::Allow
            } else if s.any_dynamic() || s.has(&["--pathspec-from-file"]) {
                Action::Dynamic
            } else if s.all().next().is_none() && s.has(&["-p", "--patch"]) {
                // `restore -p` with no paths goes through the whole tree.
                Action::RestorePaths(vec![".".into()])
            } else {
                Action::RestorePaths(texts(s.all()))
            }
        }
        "checkout" => {
            let s = scan(a, "bB", "", &["orphan", "conflict", "pathspec-from-file"]);
            if s.has(&["-f", "--force"]) {
                Action::Discard
            } else if s.has(&["-b", "-B", "--orphan", "--detach", "-d"]) {
                Action::Switch
            } else if s.dd {
                if s.after.iter().any(Word::is_dynamic) {
                    Action::Dynamic
                } else {
                    Action::RestorePaths(texts(s.after.iter()))
                }
            } else if s.has(&["--pathspec-from-file"]) {
                Action::Dynamic
            } else {
                match s.pos.as_slice() {
                    [] if s.has(&["-p", "--patch"]) => Action::RestorePaths(vec![".".into()]),
                    [] => Action::Allow,
                    [Word::Dynamic(_)] => Action::Dynamic,
                    [Word::Lit(n)] if n == "-" => Action::Switch,
                    [Word::Lit(n)] if n == "." => Action::RestorePaths(vec![".".into()]),
                    [Word::Lit(n)] if s.has(&["-p", "--patch"]) => {
                        Action::RestorePaths(vec![n.clone()])
                    }
                    [Word::Lit(n)] => Action::CheckoutName(n.clone()),
                    [Word::Lit(first), rest @ ..] if !rest.iter().any(Word::is_dynamic) => {
                        Action::CheckoutPaths {
                            first: first.clone(),
                            paths: texts(rest.iter()),
                        }
                    }
                    _ => Action::Dynamic,
                }
            }
        }
        "switch" => {
            let s = scan(
                a,
                "cC",
                "",
                &["create", "force-create", "orphan", "conflict"],
            );
            if s.has(&["-f", "--force", "--discard-changes"]) {
                Action::Discard
            } else {
                Action::Switch
            }
        }
        "clean" => {
            let s = scan(a, "e", "", &["exclude"]);
            if s.has(&["-n", "--dry-run"]) {
                Action::Allow
            } else {
                Action::Clean
            }
        }
        "rebase" | "merge" | "pull" | "cherry-pick" | "revert" | "am" => {
            Action::Rewrite(sub.to_string())
        }
        "apply" => {
            let s = scan(a, "", "", &[]);
            if s.has(&["--check", "--stat", "--numstat", "--summary"]) {
                Action::Allow
            } else {
                Action::Rewrite("apply".to_string())
            }
        }
        "worktree" => {
            let s = scan(a, "bB", "", &["reason", "expire"]);
            match s.pos.first() {
                Some(Word::Dynamic(_)) => Action::Dynamic,
                Some(Word::Lit(k)) if k == "remove" || k == "move" => match s.pos.get(1) {
                    Some(Word::Lit(t)) if k == "remove" => Action::WorktreeRemove(t.clone()),
                    Some(Word::Lit(t)) => Action::WorktreeMove(t.clone()),
                    Some(Word::Dynamic(_)) => Action::Dynamic,
                    None => Action::Dynamic,
                },
                _ => Action::Allow,
            }
        }
        "submodule" => {
            let s = scan(a, "", "", &[]);
            match s.pos.first() {
                Some(Word::Dynamic(_)) => Action::Dynamic,
                Some(Word::Lit(k)) if k == "update" || k == "deinit" => {
                    Action::Submodule(k.clone())
                }
                _ => Action::Allow,
            }
        }
        _ if KNOWN.contains(&sub) => Action::Allow,
        _ => Action::Unknown(sub.to_string()),
    }
}

/// Value of `alias.<name>` among the `-c` options of the call.
pub fn alias_in_config<'a>(c: &'a GitCall, name: &str) -> Option<&'a str> {
    let key = format!("alias.{name}");
    c.config
        .iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case(&key))
        .map(|(_, v)| v.as_str())
}

/// Expand an alias: `value` is the alias definition (`commit -a`). The result is the same
/// call with the alias replaced by its definition, ready for [`classify`]. Shell aliases
/// (`!…`), empty or non-literal definitions give `None`; the caller treats them as dynamic.
pub fn expand_alias(c: &GitCall, value: &str) -> Option<GitCall> {
    let v = value.trim();
    if v.is_empty() || v.starts_with('!') {
        return None;
    }
    let p = shell::parse(v, "", None);
    if p.commands.len() != 1 || !p.single {
        return None;
    }
    let words = &p.commands[0].words;
    let (first, rest) = words.split_first()?;
    first.lit().filter(|s| !s.starts_with('-'))?;
    let mut out = c.clone();
    out.sub = first.clone();
    out.args = rest.iter().cloned().chain(c.args.iter().cloned()).collect();
    Some(out)
}

/// `TCAS_ALLOW` is set for this command: as a prefix, through `env`, or by `export`.
pub fn sets_tcas_allow(s: &Simple) -> bool {
    if s.env.iter().any(|(k, _)| k == "TCAS_ALLOW") {
        return true;
    }
    let setter = s.words.first().and_then(Word::lit).map(shell::basename);
    matches!(
        setter,
        Some("export" | "declare" | "typeset" | "readonly" | "env" | "set")
    ) && s.words.iter().any(|w| {
        let t = w.text();
        t == "TCAS_ALLOW" || t.starts_with("TCAS_ALLOW=")
    })
}

/// A command run by `eval` or `sh -c` whose program is computed (`eval "$x"`,
/// `bash -c "$CMD"`): it may be git (harness.md section 5.1, "识别不了的时候").
pub fn dynamic_script(s: &Simple) -> bool {
    s.words.first().is_some_and(Word::is_dynamic)
        && s.wrapped_by.iter().any(|w| w == "eval" || w.ends_with(" -c"))
}

/// harness.md 5.5: the whole command is one plain `tcas` command. Returns its arguments.
pub fn tcas_command(p: &Parsed) -> Option<Vec<String>> {
    if !p.single || p.commands.len() != 1 {
        return None;
    }
    let s = &p.commands[0];
    if !s.wrapped_by.is_empty() || !s.redirects.is_empty() || !s.env.is_empty() {
        return None;
    }
    let (head, rest) = s.words.split_first()?;
    if shell::basename(head.lit()?) != "tcas" {
        return None;
    }
    rest.iter().map(|w| w.lit().map(str::to_string)).collect()
}

/// A `tcas lease wait` appears anywhere in the parsed commands.
pub fn has_tcas_wait(p: &Parsed) -> bool {
    p.commands.iter().any(|s| {
        let Some(head) = s.words.first().and_then(Word::lit) else {
            return false;
        };
        if shell::basename(head) != "tcas" {
            return false;
        }
        let mut it = s
            .words
            .iter()
            .skip(1)
            .filter_map(Word::lit)
            .filter(|a| !a.starts_with('-'));
        it.next() == Some("lease") && it.next() == Some("wait")
    })
}

/// The program only reads its file arguments (cat, less, head, tail, ls, stat, wc, grep,
/// rg, diff, file). Redirect targets are not covered: a caller applying the `~/.tcas` rule
/// should still look at `s.redirects`.
pub fn is_read_only(s: &Simple) -> bool {
    s.words
        .first()
        .and_then(Word::lit)
        .map(shell::basename)
        .is_some_and(|b| {
            matches!(
                b,
                "cat"
                    | "less"
                    | "head"
                    | "tail"
                    | "ls"
                    | "stat"
                    | "wc"
                    | "grep"
                    | "rg"
                    | "diff"
                    | "file"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::parse;

    const HOME: &str = "/home/u";

    /// Classify every git call in `cmd` run from `/w`; each as (dir, action).
    fn calls(cmd: &str) -> Vec<(String, Action)> {
        parse(cmd, "/w", Some(HOME))
            .commands
            .iter()
            .filter_map(git_call)
            .map(|c| (c.dir.clone(), classify(&c)))
            .collect()
    }

    fn one(cmd: &str) -> Action {
        let v = calls(cmd);
        assert_eq!(v.len(), 1, "{cmd}: {v:?}");
        v.into_iter().next().unwrap().1
    }

    fn s(x: &str) -> String {
        x.to_string()
    }

    fn sv(x: &[&str]) -> Vec<String> {
        x.iter().map(|y| y.to_string()).collect()
    }

    // ---- plan.md cases 49-54, 89 ----

    #[test]
    fn case_49_cd_then_stash() {
        assert_eq!(
            calls("cd api && git stash"),
            vec![(s("/w/api"), Action::StashSave)]
        );
    }

    #[test]
    fn case_50_subshell_cd_does_not_leak() {
        assert_eq!(
            calls("(cd web && git status); git -C api add -A"),
            vec![(s("/w/web"), Action::Allow), (s("/w/api"), Action::AddAll)]
        );
    }

    #[test]
    fn case_51_wrapped_forms() {
        assert_eq!(
            calls(r#"bash -c "cd api && git commit -am x""#),
            vec![(s("/w/api"), Action::CommitAll)]
        );
        assert_eq!(
            calls("env FOO=1 git -C api stash"),
            vec![(s("/w/api"), Action::StashSave)]
        );
        assert_eq!(
            calls("/usr/bin/git -C api reset --hard"),
            vec![(s("/w/api"), Action::ResetHard)]
        );
        assert_eq!(
            calls("/opt/homebrew/bin/git stash"),
            vec![(s("/w"), Action::StashSave)]
        );
    }

    #[test]
    fn case_52_quoted_semicolon_with_paths() {
        assert_eq!(
            calls(r#"git -C api commit -m "fix; cleanup" -- src/mine.ts"#),
            vec![(s("/w/api"), Action::CommitPaths(sv(&["src/mine.ts"])))]
        );
    }

    #[test]
    fn case_53_dynamic() {
        assert_eq!(
            calls("git -C api $SUB"),
            vec![(s("/w/api"), Action::Dynamic)]
        );
        assert_eq!(
            calls("ls | xargs git -C api add"),
            vec![(s("/w/api"), Action::Dynamic)]
        );
        assert_eq!(
            calls("find . -name x -exec git add {} \\;"),
            vec![(s("/w"), Action::Dynamic)]
        );
        assert_eq!(
            calls("git add $(git ls-files -m)").last().unwrap().1,
            Action::Dynamic
        );
        assert_eq!(one("git add *.c"), Action::Dynamic);
    }

    #[test]
    fn case_54_alias() {
        let p = parse("git -C api ca -m x", "/w", None);
        let c = git_call(&p.commands[0]).unwrap();
        assert_eq!(classify(&c), Action::Unknown(s("ca")));
        let e = expand_alias(&c, "commit -a").unwrap();
        assert_eq!(classify(&e), Action::CommitAll);
        assert_eq!(e.dir, "/w/api");
        assert_eq!(
            e.args,
            vec![Word::Lit(s("-a")), Word::Lit(s("-m")), Word::Lit(s("x"))]
        );

        let p = parse("git -c alias.st='stash push' st", "/w", None);
        let c = git_call(&p.commands[0]).unwrap();
        assert_eq!(c.config, vec![(s("alias.st"), s("stash push"))]);
        let v = alias_in_config(&c, "st").unwrap();
        assert_eq!(classify(&expand_alias(&c, v).unwrap()), Action::StashSave);

        assert!(expand_alias(&c, "!git stash").is_none());
        assert!(expand_alias(&c, "").is_none());
        assert!(expand_alias(&c, "$X y").is_none());
        assert!(expand_alias(&c, "a; b").is_none());
        // alias to alias: caller loops
        let c = git_call(&parse("git x", "/w", None).commands[0]).unwrap();
        let e = expand_alias(&c, "y -q").unwrap();
        assert_eq!(classify(&e), Action::Unknown(s("y")));
    }

    #[test]
    fn case_89_relative_up() {
        assert_eq!(
            calls("cd ../app-backend && git stash"),
            vec![(s("/app-backend"), Action::StashSave)]
        );
        let r = parse("cd ../app-backend && git stash", "/ws/app-front", None);
        assert_eq!(git_call(&r.commands[1]).unwrap().dir, "/ws/app-backend");
    }

    // ---- 5.1 rules: should block ----

    #[test]
    fn add_all_forms() {
        for c in [
            "git add -A",
            "git add --all",
            "git add -u",
            "git add --update",
            "git add .",
            "git add :/",
            "git add -uA",
            "git add -f -A x",
            "git add -- .",
            "git add -A $X",
        ] {
            assert_eq!(one(c), Action::AddAll, "{c}");
        }
    }

    #[test]
    fn add_paths() {
        assert_eq!(
            one("git add a.ts b/c.ts"),
            Action::AddPaths(sv(&["a.ts", "b/c.ts"]))
        );
        assert_eq!(one("git add -- a.ts"), Action::AddPaths(sv(&["a.ts"])));
        assert_eq!(one("git add -N a.ts"), Action::AddPaths(sv(&["a.ts"])));
        assert_eq!(one("git add src"), Action::AddPaths(sv(&["src"])));
        assert_eq!(one("git add ./"), Action::AddAll);
        assert_eq!(one("git add -p"), Action::AddAll);
        assert_eq!(one("git add -p a.ts"), Action::AddPaths(sv(&["a.ts"])));
        assert_eq!(one("git add --pathspec-from-file=x"), Action::Dynamic);
        assert_eq!(one("git add a $f"), Action::Dynamic);
    }

    #[test]
    fn commit_forms() {
        for (c, want) in [
            ("git commit -a -m x", Action::CommitAll),
            ("git commit --all -m x", Action::CommitAll),
            ("git commit -am x", Action::CommitAll),
            ("git commit -m x -a", Action::CommitAll),
            ("git commit -sam x", Action::CommitAll),
            ("git commit -n -m x", Action::CommitNoVerify),
            ("git commit --no-verify -m x", Action::CommitNoVerify),
            ("git commit -nm x", Action::CommitNoVerify),
            ("git commit -anm x", Action::CommitNoVerify),
            ("git commit -a --no-verify", Action::CommitNoVerify),
            ("git commit -m x", Action::CommitStaged { amend: false }),
            ("git commit", Action::CommitStaged { amend: false }),
            ("git commit --amend", Action::CommitStaged { amend: true }),
            (
                "git commit --amend --no-edit",
                Action::CommitStaged { amend: true },
            ),
            (
                "git commit -m \"$MSG\"",
                Action::CommitStaged { amend: false },
            ),
            (
                "git commit -F msg.txt",
                Action::CommitStaged { amend: false },
            ),
            (
                "git commit -m a -m b",
                Action::CommitStaged { amend: false },
            ),
            ("git commit -m -n", Action::CommitStaged { amend: false }),
            (
                "git commit -uno -m x",
                Action::CommitStaged { amend: false },
            ),
            (
                "git commit --author 'a <b>' -m x",
                Action::CommitStaged { amend: false },
            ),
            (
                "git commit -m x -- a b",
                Action::CommitPaths(sv(&["a", "b"])),
            ),
            ("git commit -m x a", Action::CommitPaths(sv(&["a"]))),
            ("git commit --only -m x a", Action::CommitPaths(sv(&["a"]))),
            ("git commit -m x -- $F", Action::Dynamic),
        ] {
            assert_eq!(one(c), want, "{c}");
        }
    }

    #[test]
    fn stash_forms() {
        for c in [
            "git stash",
            "git stash push",
            "git stash save wip",
            "git stash -u",
            "git stash -a",
            "git stash -m x",
            "git stash push -m x -- a",
            "git stash --include-untracked",
            "git stash wip",
            "git stash -k -u",
        ] {
            assert_eq!(one(c), Action::StashSave, "{c}");
        }
        for c in [
            "git stash list",
            "git stash show -p",
            "git stash pop",
            "git stash apply stash@{1}",
            "git stash drop",
        ] {
            assert_eq!(one(c), Action::Allow, "{c}");
        }
        assert_eq!(one("git stash $X"), Action::Dynamic);
        assert_eq!(one("git stash branch b"), Action::Switch);
    }

    #[test]
    fn reset_forms() {
        for c in [
            "git reset --hard",
            "git reset --hard HEAD~1",
            "git reset --merge",
            "git reset --keep x",
        ] {
            assert_eq!(one(c), Action::ResetHard, "{c}");
        }
        for c in [
            "git reset",
            "git reset --soft HEAD~1",
            "git reset --mixed",
            "git reset HEAD a.ts",
        ] {
            assert_eq!(one(c), Action::Allow, "{c}");
        }
    }

    #[test]
    fn restore_and_checkout_paths() {
        assert_eq!(one("git checkout ."), Action::RestorePaths(sv(&["."])));
        assert_eq!(one("git restore ."), Action::RestorePaths(sv(&["."])));
        assert_eq!(
            one("git checkout -- a b"),
            Action::RestorePaths(sv(&["a", "b"]))
        );
        assert_eq!(
            one("git checkout HEAD -- a"),
            Action::RestorePaths(sv(&["a"]))
        );
        assert_eq!(one("git checkout HEAD a"), Action::CheckoutPaths { first: s("HEAD"), paths: sv(&["a"]) });
        assert_eq!(one("git restore a"), Action::RestorePaths(sv(&["a"])));
        assert_eq!(
            one("git restore -s HEAD~1 a"),
            Action::RestorePaths(sv(&["a"]))
        );
        assert_eq!(
            one("git restore --source=HEAD a"),
            Action::RestorePaths(sv(&["a"]))
        );
        assert_eq!(
            one("git restore --staged --worktree a"),
            Action::RestorePaths(sv(&["a"]))
        );
        assert_eq!(one("git restore -SW a"), Action::RestorePaths(sv(&["a"])));
        assert_eq!(one("git restore --staged a"), Action::Allow);
        assert_eq!(one("git restore -S a"), Action::Allow);
        assert_eq!(one("git restore $f"), Action::Dynamic);
        assert_eq!(one("git checkout -- $f"), Action::Dynamic);
        assert_eq!(one("git checkout -p a"), Action::RestorePaths(sv(&["a"])));
    }

    #[test]
    fn discard_forms() {
        for c in [
            "git checkout -f",
            "git checkout --force main",
            "git checkout -fb x",
            "git switch -f x",
            "git switch --force x",
            "git switch --discard-changes x",
        ] {
            assert_eq!(one(c), Action::Discard, "{c}");
        }
    }

    #[test]
    fn clean_forms() {
        assert_eq!(one("git clean -fd"), Action::Clean);
        assert_eq!(one("git clean"), Action::Clean);
        assert_eq!(one("git clean -fdx -e keep"), Action::Clean);
        assert_eq!(one("git clean -n"), Action::Allow);
        assert_eq!(one("git clean -nd"), Action::Allow);
        assert_eq!(one("git clean --dry-run -d"), Action::Allow);
        assert_eq!(one("git clean -fdn"), Action::Allow);
    }

    #[test]
    fn switch_forms() {
        for c in [
            "git switch main",
            "git switch -c new",
            "git switch -C new origin/x",
            "git switch --detach HEAD~1",
            "git switch -",
            "git checkout -b new",
            "git checkout -B new main",
            "git checkout --detach",
            "git checkout --orphan x",
            "git checkout -",
            "git checkout -b new -- ",
        ] {
            assert_eq!(one(c), Action::Switch, "{c}");
        }
        assert_eq!(one("git checkout main"), Action::CheckoutName(s("main")));
        assert_eq!(
            one("git checkout feature/x"),
            Action::CheckoutName(s("feature/x"))
        );
        assert_eq!(one("git checkout $B"), Action::Dynamic);
        assert_eq!(one("git checkout"), Action::Allow);
    }

    #[test]
    fn rewrite_forms() {
        for c in ["rebase", "merge", "pull", "cherry-pick", "revert", "am"] {
            assert_eq!(one(&format!("git {c} x")), Action::Rewrite(s(c)), "{c}");
        }
        assert_eq!(one("git rebase --abort"), Action::Rewrite(s("rebase")));
        assert_eq!(one("git apply p.diff"), Action::Rewrite(s("apply")));
        assert_eq!(
            one("git apply --cached p.diff"),
            Action::Rewrite(s("apply"))
        );
        assert_eq!(one("git apply --check p.diff"), Action::Allow);
        assert_eq!(one("git apply --stat p.diff"), Action::Allow);
    }

    #[test]
    fn worktree_and_submodule() {
        assert_eq!(
            one("git worktree remove ../wt"),
            Action::WorktreeRemove(s("../wt"))
        );
        assert_eq!(
            one("git worktree remove -f ../wt"),
            Action::WorktreeRemove(s("../wt"))
        );
        assert_eq!(
            one("git worktree move ../a ../b"),
            Action::WorktreeMove(s("../a"))
        );
        assert_eq!(one("git worktree remove $X"), Action::Dynamic);
        assert_eq!(one("git worktree add ../wt"), Action::Allow);
        assert_eq!(one("git worktree add -b br ../wt"), Action::Allow);
        assert_eq!(one("git worktree list"), Action::Allow);
        assert_eq!(one("git worktree prune"), Action::Allow);
        assert_eq!(
            one("git submodule update --init"),
            Action::Submodule(s("update"))
        );
        assert_eq!(
            one("git submodule deinit -f x"),
            Action::Submodule(s("deinit"))
        );
        assert_eq!(one("git submodule status"), Action::Allow);
        assert_eq!(one("git submodule"), Action::Allow);
    }

    // ---- 5.1: should not block ----

    #[test]
    fn allowed_commands() {
        for c in [
            "git status",
            "git status -sb",
            "git diff HEAD",
            "git log --oneline",
            "git show HEAD",
            "git blame a.ts",
            "git grep foo",
            "git fetch --all",
            "git push origin main",
            "git branch",
            "git branch new",
            "git branch -a",
            "git tag v1",
            "git rev-parse HEAD",
            "git ls-files",
            "git remote -v",
            "git config user.name",
        ] {
            assert_eq!(one(c), Action::Allow, "{c}");
        }
        assert_eq!(
            one("git pull-request-not-real-but-unknown-is-not-allow-yet"),
            Action::Unknown(s("pull-request-not-real-but-unknown-is-not-allow-yet"))
        );
    }

    // ---- global options and git_call ----

    #[test]
    fn global_options() {
        let c =
            |cmd: &str| git_call(parse(cmd, "/w", Some(HOME)).commands.last().unwrap()).unwrap();
        let g = c(
            "git --no-pager -p --paginate --no-optional-locks --bare --literal-pathspecs --exec-path -P status",
        );
        assert_eq!(g.sub, Word::Lit(s("status")));
        let g = c("git -C a -C b -c x.y=z -c k --git-dir=/g --work-tree /t stash");
        assert_eq!(g.dir, "/w/a/b");
        assert_eq!(g.config, vec![(s("x.y"), s("z")), (s("k"), s(""))]);
        assert_eq!(g.git_dir, Some(s("/g")));
        assert_eq!(g.work_tree, Some(s("/t")));
        assert_eq!(classify(&g), Action::StashSave);
        let g = c("git -C /abs -C ../x status");
        assert_eq!(g.dir, "/x");
        let g = c("git -C $D stash");
        assert!(!g.dir_known);
        let g = c("git --git-dir=$G status");
        assert!(!g.dir_known);
        let g = c("cd $X; git status");
        assert!(!g.dir_known);
        let g = parse("cd $X; git -C /abs status", "/w", None);
        assert!(git_call(&g.commands[1]).unwrap().dir_known);
        let g = c("FOO=1 git status");
        assert_eq!(g.env, vec![(s("FOO"), Word::Lit(s("1")))]);
        assert_eq!(
            c("git --git-dir .git --work-tree=. add -A").work_tree,
            Some(s("."))
        );
        assert_eq!(classify(&c("git --git-dir .git add -A")), Action::AddAll);
    }

    #[test]
    fn git_call_rejects_non_git() {
        for cmd in [
            "ls",
            "make git",
            "gitk",
            "echo git stash",
            "FOO=1",
            "git",
            "git -C",
            "git --version",
        ] {
            let r = parse(cmd, "/w", None);
            assert!(r.commands.first().and_then(git_call).is_none(), "{cmd}");
        }
    }

    #[test]
    fn git_in_other_wrappers() {
        assert_eq!(one("sudo -u x git stash"), Action::StashSave);
        assert_eq!(one("command git stash"), Action::StashSave);
        assert_eq!(one("exec git stash"), Action::StashSave);
        assert_eq!(one("time git stash"), Action::StashSave);
        assert_eq!(one("nice -n 5 git stash"), Action::StashSave);
        assert_eq!(one("nohup git stash"), Action::StashSave);
        assert_eq!(one("eval 'git stash'"), Action::StashSave);
        assert_eq!(one("sh -c 'git stash'"), Action::StashSave);
        assert_eq!(one("zsh -c \"git stash\""), Action::StashSave);
        assert_eq!(one("echo $(git stash)"), Action::StashSave);
        assert_eq!(one("echo x | git stash"), Action::StashSave);
        assert_eq!(one("if true; then git stash; fi"), Action::StashSave);
        assert_eq!(
            calls("git status && git stash"),
            vec![(s("/w"), Action::Allow), (s("/w"), Action::StashSave)]
        );
    }

    // ---- TCAS_ALLOW ----

    #[test]
    fn tcas_allow() {
        let any = |cmd: &str| parse(cmd, "/w", None).commands.iter().any(sets_tcas_allow);
        assert!(any("TCAS_ALLOW=1 git commit"));
        assert!(any("env TCAS_ALLOW=1 git commit"));
        assert!(any("export TCAS_ALLOW=1"));
        assert!(any("export TCAS_ALLOW=1; git commit"));
        assert!(any("bash -c 'TCAS_ALLOW=1 git commit'"));
        assert!(any("TCAS_ALLOW=1"));
        assert!(any("declare -x TCAS_ALLOW=1"));
        assert!(any("export TCAS_ALLOW"));
        assert!(!any("echo TCAS_ALLOW"));
        assert!(!any("git commit"));
        let r = parse("export TCAS_ALLOW=1; git commit", "/w", None);
        assert!(sets_tcas_allow(&r.commands[1]));
    }

    // ---- 5.5 tcas commands ----

    #[test]
    fn tcas_commands() {
        let t = |cmd: &str| tcas_command(&parse(cmd, "/w", None));
        assert_eq!(
            t("tcas lease wait a.txt"),
            Some(sv(&["lease", "wait", "a.txt"]))
        );
        assert_eq!(t("/usr/local/bin/tcas status"), Some(sv(&["status"])));
        assert_eq!(
            t("tcas lease acquire 'a b'"),
            Some(sv(&["lease", "acquire", "a b"]))
        );
        assert_eq!(
            t("tcas lease wait a.txt # c"),
            Some(sv(&["lease", "wait", "a.txt"]))
        );
        assert_eq!(t("tcas lease wait a.txt && echo hi"), None);
        assert_eq!(t("tcas status > x"), None);
        assert_eq!(t("tcas status | cat"), None);
        assert_eq!(t("tcas status; ls"), None);
        assert_eq!(t("tcas status &"), None);
        assert_eq!(t("tcas status $(x)"), None);
        assert_eq!(t("tcas status $X"), None);
        assert_eq!(t("tcas status `x`"), None);
        assert_eq!(t("(tcas status)"), None);
        assert_eq!(t("sudo tcas status"), None);
        assert_eq!(t("X=1 tcas status"), None);
        assert_eq!(t("bash -c 'tcas status'"), None);
        assert_eq!(t("tcasx status"), None);
        assert_eq!(t("git status"), None);
        assert_eq!(t(""), None);

        let w = |cmd: &str| has_tcas_wait(&parse(cmd, "/w", None));
        assert!(w("tcas lease wait a.txt"));
        assert!(w("tcas lease wait a.txt && echo hi"));
        assert!(w("echo hi; tcas lease wait a.txt"));
        assert!(w("tcas --json lease wait a"));
        assert!(w("bash -c 'tcas lease wait a'"));
        assert!(!w("tcas lease acquire a"));
        assert!(!w("tcas status"));
        assert!(!w("echo tcas lease wait"));
        assert!(!w("git status"));
    }

    #[test]
    fn read_only_programs() {
        let r = |cmd: &str| is_read_only(&parse(cmd, "/w", None).commands[0]);
        for c in [
            "cat", "less", "head", "tail", "ls", "stat", "wc", "grep", "rg", "diff", "file",
        ] {
            assert!(r(&format!("{c} ~/.tcas/x")), "{c}");
        }
        assert!(r("/bin/cat x"));
        assert!(r("sudo cat x"));
        for c in [
            "rm ~/.tcas/x",
            "vim x",
            "sed -i s/a/b/ x",
            "cp a b",
            "tee x",
            "$X y",
            "echo hi",
        ] {
            assert!(!r(c), "{c}");
        }
        let p = parse("cat a > ~/.tcas/x", "/w", Some(HOME));
        assert!(is_read_only(&p.commands[0]));
        assert_eq!(
            p.commands[0].redirects,
            vec![Word::Lit(s("/home/u/.tcas/x"))]
        );
    }

    // ---- shell-level inputs the guard must survive ----

    #[test]
    fn heredoc_and_comments() {
        assert!(calls("cat <<EOF\ngit stash\nEOF").is_empty());
        assert_eq!(
            calls("git commit -F - <<EOF\nmsg; git stash\nEOF\ngit stash"),
            vec![
                (s("/w"), Action::CommitStaged { amend: false }),
                (s("/w"), Action::StashSave)
            ]
        );
        assert!(calls("# git stash").is_empty());
        assert!(calls("echo hi # git stash").is_empty());
        assert!(calls("echo 'git stash'").is_empty());
    }

    #[test]
    fn malformed_input() {
        for c in [
            "git",
            "git -C",
            "git -c",
            "git commit -m",
            "git commit -",
            "git commit \"",
            "git stash '",
            "git add \"unterminated; git stash",
            "git $(",
            "git --git-dir",
            "git checkout --",
            "git worktree remove",
            "git worktree",
            "git -C a -C",
            "bash -c \"git stash",
            "git reset --",
        ] {
            let p = parse(c, "/w", None);
            for s in &p.commands {
                if let Some(g) = git_call(s) {
                    let _ = classify(&g);
                }
            }
        }
        assert_eq!(
            calls("git add \"unterminated; git stash"),
            vec![(s("/w"), Action::AddPaths(sv(&["unterminated; git stash"])))]
        );
        assert_eq!(
            calls("bash -c \"git stash"),
            vec![(s("/w"), Action::StashSave)]
        );
    }

    // ---- forms found in the audit of 2026-11-25 ----

    #[test]
    fn computed_scripts_are_dynamic() {
        for cmd in [r#"eval "$x""#, r#"bash -c "$CMD""#, r#"sh -c "$(cat script)""#] {
            let p = parse(cmd, "/w", Some(HOME));
            assert!(p.commands.iter().any(dynamic_script), "{cmd}: {p:?}");
        }
        for cmd in [r#"bash -c "echo $HOME""#, "$EDITOR a.txt", r#"eval "echo $x""#] {
            let p = parse(cmd, "/w", Some(HOME));
            assert!(!p.commands.iter().any(dynamic_script), "{cmd}: {p:?}");
        }
        assert_eq!(calls(r#"bash -c "cd api && echo $X && git stash""#), vec![(s("/w/api"), Action::StashSave)]);
        assert_eq!(one(r#"sh -c "git $SUB""#), Action::Dynamic);
    }

    #[test]
    fn checkout_with_several_paths_keeps_the_first() {
        assert_eq!(
            one("git checkout other.rs mine.rs"),
            Action::CheckoutPaths { first: s("other.rs"), paths: sv(&["mine.rs"]) }
        );
        assert_eq!(
            one("git checkout main src/a.ts src/b.ts"),
            Action::CheckoutPaths { first: s("main"), paths: sv(&["src/a.ts", "src/b.ts"]) }
        );
        assert_eq!(one("git checkout $REF a.ts"), Action::Dynamic);
        assert_eq!(one("git checkout main $F"), Action::Dynamic);
    }

    #[test]
    fn wrappers_that_change_directory() {
        assert_eq!(calls("env -C api git stash"), vec![(s("/w/api"), Action::StashSave)]);
        assert_eq!(calls("sudo --chdir=api git reset --hard"), vec![(s("/w/api"), Action::ResetHard)]);
        assert_eq!(calls(r#"env -S "git -C api stash""#), vec![(s("/w/api"), Action::StashSave)]);
        assert_eq!(calls("bash -c -- 'cd api && git stash'"), vec![(s("/w/api"), Action::StashSave)]);
    }

    #[test]
    fn computed_option_values_are_not_paths() {
        assert_eq!(one(r#"git commit --message="$MSG" -- mine.rs"#), Action::CommitPaths(sv(&["mine.rs"])));
        assert_eq!(one(r#"git commit -m"$MSG" -- mine.rs"#), Action::CommitPaths(sv(&["mine.rs"])));
        assert_eq!(one(r#"git commit --author="$A" -m x"#), Action::CommitStaged { amend: false });
        assert_eq!(one(r#"git commit --pathspec-from-file="$F""#), Action::Dynamic);
        assert_eq!(one("git commit -- $F"), Action::Dynamic);
    }

    #[test]
    fn patch_and_include_modes() {
        assert_eq!(one("git checkout -p"), Action::RestorePaths(sv(&["."])));
        assert_eq!(one("git restore --patch"), Action::RestorePaths(sv(&["."])));
        assert_eq!(one("git restore -p a.ts"), Action::RestorePaths(sv(&["a.ts"])));
        assert_eq!(one("git commit -i mine.rs -m x"), Action::CommitStaged { amend: false });
        assert_eq!(one("git commit --include mine.rs -m x"), Action::CommitStaged { amend: false });
        assert_eq!(one("git commit -o mine.rs -m x"), Action::CommitPaths(sv(&["mine.rs"])));
    }

    #[test]
    fn whole_tree_pathspecs_add_everything() {
        for p in [".", "./", ":/", ":(top)", ":/.", ":(top).", ":(top)./"] {
            assert_eq!(one(&format!("git add '{p}'")), Action::AddAll, "{p}");
        }
        assert_eq!(one("git add ./src"), Action::AddPaths(sv(&["./src"])));
    }

    #[test]
    fn computed_alias_in_config_is_dynamic() {
        let p = parse("git -c alias.st=$X st", "/w", None);
        let c = git_call(&p.commands[0]).unwrap();
        assert!(!c.dir_known);
        let p = parse(r#"git -c user.name="$N" commit -m x -- a.ts"#, "/w", None);
        let c = git_call(&p.commands[0]).unwrap();
        assert!(c.dir_known);
        assert_eq!(classify(&c), Action::CommitPaths(sv(&["a.ts"])));
    }

    #[test]
    fn work_dir_follows_work_tree_and_git_dir() {
        let wd = |cmd: &str| git_call(&parse(cmd, "/w", None).commands[0]).unwrap().work_dir();
        assert_eq!(wd("git stash"), "/w");
        assert_eq!(wd("git --work-tree=api stash"), "/w/api");
        assert_eq!(wd("git --git-dir=api/.git stash"), "/w/api");
        assert_eq!(wd("git --git-dir /r/app/.git --work-tree /r/app stash"), "/r/app");
        assert_eq!(wd("git -C api --git-dir=.git stash"), "/w/api");
        assert_eq!(wd("git --git-dir=/srv/bare.git log"), "/srv/bare.git");
    }
}
