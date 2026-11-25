# TCAS

**Collision avoidance for coding agents that share a checkout.**

[中文](README.zh-CN.md)

Run several Claude Code or Antigravity CLI sessions in the same repository and sooner or later two of them edit the same file, or one runs `git add -A`, `git stash` or `git checkout .` over the other's half-finished work. TCAS stops that. It gives every file **one writer at a time**: a session that tries to change a file another session is working on is refused on the spot, told who holds it and why, and can wait for it in the background while it gets on with something else.

```
tcas: src/auth/session.ts is leased by another session (claude:7958…, since 14:02).
  Their brief: "Fix the login redirect loop in src/auth"
It has uncommitted work in progress. Do not modify it by any other means, including Bash.
If their brief already covers the change you were about to make, do not wait: tell the user.
To wait for it, run this as a background command (Bash tool, run_in_background):
  tcas lease wait src/auth/session.ts --brief "<one line: what you need it for>"
```

*What an agent sees when it tries to edit a file another session is changing.*

## Features

- **Leases on files and directories.** A session takes a file's lease the first time it edits it; nothing to learn for the agent. Before a larger change it can claim a whole directory up front, all or nothing, with a one-line `--brief` that other sessions see.
- **Refused before the write, not after.** Edit tools are checked in the harness's `PreToolUse` hook. If tcasd is unreachable, edits inside a workspace are refused rather than let through.
- **Waiting without blocking.** `tcas lease wait` runs as a background command; the session is woken when the lease is granted. Deadlocks between waiting sessions are detected and broken.
- **Shell commands are covered too.** A command guard refuses `git add -A`, `git commit -a`, `git stash`, `git reset --hard`, branch switches and the like while others hold leases in that checkout (it reads through `cd`, `bash -c`, `env`, aliases and more). Git hooks catch the same operations however they were started, and each Bash call is reconciled afterwards to see which files it changed.
- **Commits that take only your files.** `tcas commit` commits the files this session changed, one commit per checkout, leaving other sessions' staged files alone. Committing releases the leases.
- **Leases let go on their own.** A lease on a clean file gives way to the next session that asks; explicit leases yield after 30 minutes unused, or once their holder has committed and ended its turn. A file with uncommitted changes never changes hands by itself.
- **Works with how you already work.** One checkout or several, repositories nested inside a folder, worktrees, submodules, sibling directories in one workspace. Git-ignored files, temporary directories and the harnesses' own directories are left alone.
- **Measurable.** Every lease change is an event; `tcas stats` reports contention, waits, overreach and how often agents claimed files before editing them.

## Install

TCAS is built from source and needs Rust 1.88 or later.

```sh
git clone https://github.com/Jaciezyt/tcas.git
cd tcas
cargo install --path crates/tcas     # installs tcas and tcasd into ~/.cargo/bin
```

Then add the plugin to the harnesses you use. This needs the harness's own command (`claude` or `agy`) on your `PATH`.

```sh
tcas plugin install claude
tcas plugin install agy
tcas doctor                          # checks the daemon, plugins, git hooks and configuration
```

Restart running sessions so they load the plugin. The plugin holds only hook configuration, a small script that forwards to your installed `tcas`, and usage notes for the agent; upgrading `tcas` does not require reinstalling it.

## Quick start

Tell TCAS which directory to protect. Run this in a repository, or in a folder that holds several:

```console
$ cd ~/Code/app
$ tcas workspace open
opened /Users/me/Code/app (mode auto), 1 checkout:
  .           repo       main
```

That is all the setup there is. Start your sessions as usual; they take and release leases as they edit and commit. The daemon `tcasd` starts on first use.

To see what is going on:

```console
$ tcas status
workspace  /Users/me/Code/app   mode auto   branch main

SESSION         STATE   LAST   TITLE                     LEASES
claude:1cce03d4 active  now    add export button         1
claude:7958a1b2 active  now    fix login redirect        1

claude:7958a1b2
  src/auth/                   cli    14:02   dir, 1 file, used 0s ago
  brief: "Fix the login redirect loop in src/auth"
…

$ tcas lease who src/auth/session.ts
src/auth/session.ts  src/auth/ leased by claude:7958a1b2 since 14:02 (cli, dir, 1 file, used 0s ago)
                       their brief: "Fix the login redirect loop in src/auth"
```

Your own edits are not restricted. Your commits are checked, though: if a commit would include a file a session is still changing, the git hook refuses it and names the file. Commit only your own paths, or override once with `TCAS_ALLOW=1 git commit …`.

## Commands

| Command | Who | What it does |
|---|---|---|
| `tcas workspace open [dir…]` | user, agent | Start protecting a checkout or a folder of checkouts |
| `tcas workspace add <dir>` / `remove <dir>` | user, agent | Add or remove a sibling directory or an outside worktree |
| `tcas workspace close` | user, agent | Stop protecting a workspace (an agent only one it opened, once empty) |
| `tcas workspace list` | anyone | List workspaces and who opened them |
| `tcas workspace set mode auto\|strict` | user | `strict`: sessions must claim files before editing them |
| `tcas status` | anyone | Sessions, their leases, stalled and orphaned leases, unattributed changes |
| `tcas lease acquire <path…> --brief "…"` | agent | Claim files or directories up front, all or nothing |
| `tcas lease wait <path…> --brief "…"` | agent | Wait until all of them can be granted (run in the background) |
| `tcas lease release [<path…>\|--all]` | agent | Give leases back |
| `tcas lease list` / `who <path…>` | anyone | Who holds what, and why a path is not managed |
| `tcas lease grant` / `transfer` / `release --force` | user | Hand out, move or revoke a session's leases |
| `tcas commit -m "…" [<path…>]` | agent | Commit only this session's changed files |
| `tcas session list` | anyone | Sessions and what they hold |
| `tcas session end <session>` | user | End a session by hand |
| `tcas log` / `tcas stats` | anyone | The event log; contention and usage statistics |
| `tcas exempt list` | anyone | Directories TCAS never manages, and why |
| `tcas exempt add` / `remove <dir>` | user | Change that list |
| `tcas plugin install <harness>` / `tcas doctor` | user | Install plugins; check the installation |
| `tcas daemon start\|stop\|status` | user | Control `tcasd` (it starts by itself when needed) |

Every command takes `--json`. Exit codes: `0` success, `1` error, `3` lease denied, `4` wait timed out, `5` deadlock, `6` user-only command.

TCAS tells an agent what it may do. Commands that only add protection or only read are open to agents (and hooks approve the safe ones automatically); commands that could be used to get around another session's lease are the user's alone. A session is recognized by its environment and by its ancestor processes, so clearing environment variables does not make an agent the user.

## How it works

```
 your terminal           harness session A          harness session B
   │ tcas …                │ plugin hooks              │ plugin hooks
   │                       │ tcas hook / tcas lease …  │
   └──────────┬────────────┴─────────────┬─────────────┘
              │  Unix socket ~/.tcas/tcasd.sock
        ┌─────▼──────────────────────────────────────┐
        │ tcasd: workspaces · sessions · leases ·    │  ~/.tcas/tcas.db (SQLite)
        │        waits · event log                   │
        └─────┬──────────────────────────────────────┘
              │ git status, check-ignore (read-only)
        your checkouts (shared by you and every session)
```

- **`tcas`** is the command line for you and the agents, and the program the harness hooks and git hooks call.
- **`tcasd`** is a per-user daemon. It keeps the lease table in SQLite and makes every decision in one transaction. It never writes to your working tree; the only thing it changes in a repository is one `include.path` line in `.git/config` for the git hooks.
- **Edits** are checked before they happen (`PreToolUse`). **Shell commands** cannot be stopped from writing files, so they get three layers: the command guard before the command runs, git hooks for commits, rebases and stashes, and a git-status snapshot before and after each call to see what changed. A write to a file someone else holds is reported to both sessions, never reverted.

The full design, the rules and the reasoning behind them are in [`docs/v1/`](docs/v1/README.md) (Chinese).

## Configuration

Defaults work without any configuration. Global settings live in `~/.tcas/config.toml`; per-workspace settings in a `.tcas.toml` at the workspace root, which you can commit:

```toml
mode = "auto"                         # auto | strict (claim before editing)
roots = ["../app-backend"]            # sibling directories that belong to this workspace
exempt = ["scratch/"]                 # directories TCAS leaves alone
unmanaged = ["pnpm-lock.yaml"]        # tracked files that need no lease (generated files)

[lease]
explicit_idle_secs = 1800             # an unused explicit lease may yield after this
max_dir_files = 200                   # largest directory a session may claim at once

[git_hooks]
check_user = true                     # also check your own commits
```

Every option is described in [`docs/v1/daemon-and-cli.md`](docs/v1/daemon-and-cli.md#6-配置). Agents cannot change these files: edits to `~/.tcas` and `.tcas.toml` are refused.

To run a single session without TCAS, start it with `TCAS_DISABLE=1`.

## Limits

TCAS guards against agents getting in each other's way by accident; it is not a sandbox.

- Shell commands can still write files someone else holds. Such writes are found afterwards and reported, not prevented.
- Git-ignored files (`.env.local`, build output) are not protected.
- Your own edits are not restricted, only your commits are checked.
- A session that deliberately strips both its environment and its process ancestry is treated as the user.
- Directories outside every workspace are not managed; sessions are told once when they edit a checkout there.

See [`docs/v1/leases.md`](docs/v1/leases.md), section 9, for the complete list.

## Documentation

| | |
|---|---|
| [docs/v1/README.md](docs/v1/README.md) | Overview, scope and the design choices behind them |
| [docs/v1/leases.md](docs/v1/leases.md) | The lease model: paths, acquiring, releasing, waiting, invariants |
| [docs/v1/harness.md](docs/v1/harness.md) | Plugins, every hook, failure handling, measured harness behavior |
| [docs/v1/daemon-and-cli.md](docs/v1/daemon-and-cli.md) | The daemon, storage, protocol, every command, configuration |
| [docs/v1/walkthrough.md](docs/v1/walkthrough.md) | Three sessions and the user working in parallel, step by step |
| [docs/v1/plan.md](docs/v1/plan.md) | Milestones, acceptance cases, risks |
| [tests/probe/RESULTS.md](tests/probe/RESULTS.md) | Measured Claude Code and AGY hook behavior |

The documentation is in Chinese.

## Development

```sh
cargo build
cargo test --workspace      # unit, property and end-to-end tests
cargo clippy --workspace --all-targets
```

The end-to-end tests start real `tcasd` processes over Unix sockets in temporary git repositories; they fail inside sandboxes that block local sockets. Harness probes, which run real Claude Code and AGY sessions, are in `tests/probe/` (`tests/probe/run.sh`).

| Directory | Contents |
|---|---|
| `crates/tcas-core` | Pure logic, no I/O: paths, lease decisions, the wait graph, shell parsing and the command guard rules, protocol types |
| `crates/tcas-sys` | Operating-system access: clock, processes, file system, the `~/.tcas` home |
| `crates/tcasd` | The daemon: storage, request handlers, git queries, reconciliation, git hooks |
| `crates/tcas` | The command line and both program entry points; end-to-end tests in `tests/` |
| `plugins/` | The Claude Code and AGY plugins |
| `tests/probe/` | Probe plugins and scripts that measure harness behavior |

## License

[MIT](LICENSE)
