# TCAS

**让共用同一个 checkout 的 coding agent 互不相撞。**

[English](README.md)

在同一个仓库里同时开几个 Claude Code 或 Antigravity CLI 会话，迟早会有两个会话改同一个文件，或者一个会话执行 `git add -A`、`git stash`、`git checkout .`，把另一个会话改到一半的东西一起带走。TCAS 解决这个问题：它保证每个文件**同一时刻只有一个会话在改**。一个会话去改别的会话正在改的文件时当场被拒，并被告知谁占着、对方在做什么；它可以在后台等这个文件空出来，同时去做别的事。

```
tcas: src/auth/session.ts is leased by another session (claude:7958…, since 14:02).
  Their brief: "Fix the login redirect loop in src/auth"
It has uncommitted work in progress. Do not modify it by any other means, including Bash.
If their brief already covers the change you were about to make, do not wait: tell the user.
To wait for it, run this as a background command (Bash tool, run_in_background):
  tcas lease wait src/auth/session.ts --brief "<one line: what you need it for>"
```

*agent 去改另一个会话正在改的文件时看到的内容。*

## 功能

- **按文件和目录发租约**。会话第一次改某个文件时自动取得它的租约，agent 不需要学任何新东西。做较大的改动前，可以事先按目录申请，全有或全无，并用一句 `--brief` 告诉别的会话自己在做什么。
- **写之前就拒绝，而不是事后发现**。编辑工具在 harness 的 `PreToolUse` hook 里检查。tcasd 连不上时，workspace 里的修改一律拒绝，而不是放行。
- **等待不占主线程**。`tcas lease wait` 作为后台命令运行，拿到租约时会话被唤醒。等待之间的死锁会被发现并解开。
- **Shell 命令也管**。别的会话在同一个 checkout 里持有租约时，命令护栏拒绝 `git add -A`、`git commit -a`、`git stash`、`git reset --hard`、切分支等命令（能看穿 `cd`、`bash -c`、`env`、git 别名等写法）。git 钩子不论命令从哪里发起都会检查同样的操作；每次 Bash 调用结束后还会对账，看它改了哪些文件。
- **只提交自己的文件**。`tcas commit` 只提交本会话改过的文件，每个 checkout 提交一次，别的会话已经暂存的文件不受影响。提交即释放租约。
- **租约会自己让出**。干净文件上的租约会让给下一个来申请的会话；显式申请的租约 30 分钟没用到，或者持有者提交过、结束了这一轮，也会让出。有未提交改动的文件不会自动换手。
- **贴合现有的用法**。一个或几个 checkout、装着多个仓库的目录、worktree、子模块、并排的多个目录都可以放进一个 workspace；当前目录已在打开的 workspace 里时，harness 附带的额外目录（`--add-dir`、`/add-dir` 或多根工作区路径）也会自动并入。被 git 排除的文件、临时目录和 harness 自己的目录不管。
- **可度量**。租约的每次变化都有事件记录；`tcas stats` 给出争用、等待、越界，以及 agent 事先申请的比例。

## 安装

从源码构建，需要 Rust 1.88 以上。

```sh
git clone https://github.com/Jaciezyt/tcas.git
cd tcas
cargo install --path crates/tcas     # 把 tcas 和 tcasd 装到 ~/.cargo/bin
```

再给用到的 harness 装上 plugin。这一步需要 harness 自己的命令（`claude` 或 `agy`）在 `PATH` 里。

```sh
tcas plugin install claude
tcas plugin install agy
tcas doctor                          # 检查 daemon、plugin、git 钩子和配置
```

重启正在运行的会话以加载 plugin。plugin 里只有 hook 配置、一个转发到本机 `tcas` 的小脚本和给 agent 看的用法说明；升级 `tcas` 不需要重装 plugin。

## 快速开始

告诉 TCAS 要保护哪个目录。在一个仓库里运行，或者在装着几个仓库的目录里运行：

```console
$ cd ~/Code/app
$ tcas workspace open
opened /Users/me/Code/app (mode auto), 1 checkout:
  .           repo       main
```

需要设置的只有这一步。照常启动会话即可，它们改文件、提交时会自己取得和释放租约。daemon `tcasd` 在第一次用到时自动启动。

查看当前情况：

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

你自己的修改不受限制，但提交会被检查：提交里如果有会话还在改的文件，git 钩子会拒绝并列出这些文件。只提交自己的路径，或者用 `TCAS_ALLOW=1 git commit …` 放行一次。

## 命令

| 命令 | 谁能用 | 作用 |
|---|---|---|
| `tcas workspace open [目录…]` | 用户、agent | 开始保护一个 checkout，或装着几个 checkout 的目录 |
| `tcas workspace add <目录>` / `remove <目录>` | 用户、agent | 加进或去掉一个并排的目录、一个外面的 worktree |
| `tcas workspace close` | 用户、agent | 停止保护（agent 只能关自己打开的、已经空了的） |
| `tcas workspace list` | 任何人 | 列出 workspace 和是谁打开的 |
| `tcas workspace set mode auto\|strict` | 用户 | `strict`：会话必须先申请才能改 |
| `tcas status` | 任何人 | 会话、各自的租约、停滞和遗留的租约、没人持有的改动 |
| `tcas lease acquire <路径…> --brief "…"` | agent | 事先申请文件或目录，全有或全无 |
| `tcas lease wait <路径…> --brief "…"` | agent | 等到能全部授予为止（在后台运行） |
| `tcas lease release [<路径…>\|--all]` | agent | 释放租约 |
| `tcas lease list` / `who <路径…>` | 任何人 | 谁持有什么，以及一条路径为什么不归 tcas 管 |
| `tcas lease grant` / `transfer` / `release --force` | 用户 | 给会话发租约、转交、强制收回 |
| `tcas commit -m "…" [<路径…>]` | agent | 只提交本会话改过的文件 |
| `tcas session list` | 任何人 | 会话和各自持有的租约 |
| `tcas session end <会话>` | 用户 | 手动结束一个会话 |
| `tcas log` / `tcas stats` | 任何人 | 事件日志；争用和使用统计 |
| `tcas exempt list` | 任何人 | TCAS 不管的目录和来源 |
| `tcas exempt add` / `remove <目录>` | 用户 | 修改这个列表 |
| `tcas plugin install <harness>` / `tcas doctor` | 用户 | 安装 plugin；检查安装 |
| `tcas daemon start\|stop\|status` | 用户 | 控制 `tcasd`（需要时会自动启动） |

所有命令都支持 `--json`。退出码：`0` 成功，`1` 出错，`3` 租约被拒，`4` 等待超时，`5` 死锁，`6` 只有用户能用的命令。

只增加保护或只是查看的命令 agent 可以用（其中安全的由 hook 自动批准）；能被用来绕过别人租约的命令只有用户能用。会话按环境变量和祖先进程识别，所以 agent 清掉环境变量也不会被当成用户。

## 工作原理

```
 你的终端                 harness 会话 A              harness 会话 B
   │ tcas …                │ plugin 的 hook             │ plugin 的 hook
   │                       │ tcas hook / tcas lease …   │
   └──────────┬────────────┴─────────────┬──────────────┘
              │  Unix 套接字 ~/.tcas/tcasd.sock
        ┌─────▼──────────────────────────────────────┐
        │ tcasd：workspace · 会话 · 租约 · 等待 ·      │  ~/.tcas/tcas.db（SQLite）
        │        事件日志                             │
        └─────┬──────────────────────────────────────┘
              │ git status、check-ignore（只读）
        你的 checkout（你和所有会话共用）
```

- **`tcas`** 是给你和 agent 用的命令行，也是 harness 的 hook 和 git 钩子调用的程序。
- **`tcasd`** 是每个用户一个的 daemon。租约表在 SQLite 里，每次判定都在一个事务里完成。它从不写你的工作目录；它在仓库里唯一会改的，是 `.git/config` 里给 git 钩子用的一行 `include.path`。
- **编辑**在发生之前检查（`PreToolUse`）。**Shell 命令**写文件拦不住，所以分三层：命令运行前的命令护栏，管提交、变基、stash 的 git 钩子，以及每次调用前后各拍一次 git 状态快照、比出改了什么。改到别人持有的文件时通知双方，从不还原。

完整的设计、规则和背后的理由见 [`docs/v1/`](docs/v1/README.md)。

## 配置

不配置也能用。全局设置在 `~/.tcas/config.toml`；每个 workspace 的设置在它根目录下的 `.tcas.toml`，可以提交进仓库：

```toml
mode = "auto"                         # auto | strict（先申请再改）
roots = ["../app-backend"]            # 属于这个 workspace 的并排目录
exempt = ["scratch/"]                 # TCAS 不管的目录
unmanaged = ["pnpm-lock.yaml"]        # 被跟踪但不发租约的文件（生成的文件）

[lease]
explicit_idle_secs = 1800             # 显式租约多久没用到就可以让出
max_dir_files = 200                   # 一次最多能申请多大的目录

[git_hooks]
check_user = true                     # 你自己的提交也检查
```

全部选项见 [`docs/v1/daemon-and-cli.md`](docs/v1/daemon-and-cli.md#6-配置)。agent 改不了这些文件：对 `~/.tcas` 和 `.tcas.toml` 的修改会被拒绝。

想让某个会话不受 TCAS 约束，用 `TCAS_DISABLE=1` 启动它。

## 局限

TCAS 防的是 agent 无意中互相干扰，它不是沙箱。

- Shell 命令仍然能写别人持有的文件。这类写入会在事后被发现并通知，不会被阻止。
- 被 git 排除的文件（`.env.local`、构建产物）没有保护。
- 你自己的修改不受限制，只有提交会被检查。
- 同时去掉环境变量和进程祖先链的会话会被当成用户。
- 不在任何 workspace 里的目录不管；会话改到那里的 checkout 时会收到一次提示。

完整的列表见 [`docs/v1/leases.md`](docs/v1/leases.md) 第 9 节。

## 文档

| | |
|---|---|
| [docs/v1/README.md](docs/v1/README.md) | 概览、范围和设计上的取舍 |
| [docs/v1/leases.md](docs/v1/leases.md) | 租约模型：路径、申请、释放、等待、不变量 |
| [docs/v1/harness.md](docs/v1/harness.md) | plugin、每个 hook、出错时的处理、harness 的实测行为 |
| [docs/v1/daemon-and-cli.md](docs/v1/daemon-and-cli.md) | daemon、存储、协议、全部命令、配置 |
| [docs/v1/walkthrough.md](docs/v1/walkthrough.md) | 三个会话加用户并行工作的完整过程 |
| [docs/v1/plan.md](docs/v1/plan.md) | 里程碑、验收用例、风险 |
| [tests/probe/RESULTS.md](tests/probe/RESULTS.md) | Claude Code 和 AGY 的 hook 实测结果 |

## 开发

```sh
cargo build
cargo test --workspace      # 单元测试、性质测试和端到端测试
cargo clippy --workspace --all-targets
```

端到端测试会在临时 git 仓库里通过 Unix 套接字启动真实的 `tcasd`，在禁止本地套接字的沙箱里会失败。跑真实 Claude Code 和 AGY 会话的探针在 `tests/probe/`（`tests/probe/run.sh`）。

| 目录 | 内容 |
|---|---|
| `crates/tcas-core` | 纯逻辑，不做 I/O：路径、租约判定、等待图、命令解析和护栏规则、协议类型 |
| `crates/tcas-sys` | 操作系统相关：时钟、进程、文件系统、`~/.tcas` |
| `crates/tcasd` | daemon：存储、请求处理、git 查询、对账、git 钩子 |
| `crates/tcas` | 命令行和两个程序的入口；端到端测试在 `tests/` |
| `plugins/` | Claude Code 和 AGY 的 plugin |
| `tests/probe/` | 测量 harness 行为的探针 plugin 和脚本 |

## 许可证

[MIT](LICENSE)
