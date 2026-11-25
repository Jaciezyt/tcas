# TCAS

面向多 Agent 并行开发的本机文件与目录租约协调层：让你照常启动多个 coding agent 会话（Claude Code、Google Antigravity `agy` 等），在**同一个检出或多仓库项目**里同时改代码而不相撞。

主页：[tcas.io](https://tcas.io)

## 解决什么问题

在同一份工作目录里同时开两三个会话改代码时，常见撞车场景包括：两个会话同时改同一个文件、一个会话跑 `git add -A` / `git commit -a` 把别人改到一半的代码一起提交、或者 `git stash` / `git rebase` 破坏别人的工作区。

TCAS 通过原生 Harness 插件的 Hook 与仓库级 Git 钩子在后台协调所有会话：

- **默认无感互斥（`auto` 模式）**：会话改文件时自动取得租约；已被其他会话占用时当场拒绝，告知对方持有的范围与 `brief`，并给出后台等待命令（`tcas lease wait`），释放后自动唤醒。支持按需切换为强制事先申请的 `strict` 模式。
- **目录租约与意图广播**：做较大改动前用 `tcas lease acquire src/auth/ --brief "…"` 一次锁住目录并广播一句话说明，冲突在动手前暴露；内置等待环（死锁）检测与空闲/已提交租约自动让出。
- **Shell 与 Git 保护**：命令护栏拦截多会话共存时的 `git add -A`、`git stash`、`git rebase` 等全局破坏性命令；仓库配置式 Git 钩子兜底校验并在提交后自动释放租约；Bash 执行前后做快照对账归属写入。
- **按会话隔离提交**：`tcas commit -m "…"` 只暂存并提交当前会话自己改过的文件（跨多个仓库时逐仓提交），不动其他会话或用户的改动。
- **多仓库与 `/add-dir` 自动并入**：一个 workspace 可管理嵌套或并排的多个仓库（`tcas workspace add`）；当前目录已在开启的 workspace 中时，通过 `--add-dir` / `/add-dir` 或 IDE 多根工作区附带的目录会自动并入同一 workspace 持久受保。

## 支持的 Harness

| Harness | 状态 | 插件安装命令 |
|---|---|---|
| Claude Code (`claude`) | 已支持 | `tcas plugin install claude` |
| Google Antigravity CLI / IDE (`agy`) | 已支持 | `tcas plugin install agy` |
| OpenAI Codex CLI (`codex`) | 规划中（Milestone 7） | — |

不同 Harness 的会话可以在同一个 workspace 里混用并互相协调。

## 安装与快速开始

### 1. 编译安装二进制

需要 Rust 工具链（Edition 2024）与 Git（支持配置式钩子，Git 2.36+）：

```sh
cargo install --path crates/tcas
```

这会将 `tcas`（CLI 与 Hook 入口）和 `tcasd`（本机后台守护进程）安装到 `~/.cargo/bin/`。请确保安装目录在你的 `PATH` 中（也可手动复制 `target/release/{tcas,tcasd}` 到 `~/.local/bin/` 或 `~/.tcas/bin/`）。

### 2. 安装 Harness 插件并自检

```sh
# 按需安装你使用的 Harness 插件（在普通终端中运行）
tcas plugin install claude
tcas plugin install agy

# 检查守护进程、Git 版本与插件状态
tcas doctor
```

> **AGY 沙箱提示**：AGY 的 Hook 运行在沙箱外，自动租约与冲突拦截开箱即用；若希望 AGY 会话在 `run_command` 沙箱内也能直接调用 `tcas lease acquire` / `tcas commit`，可在 AGY 沙箱设置中放行 `~/.tcas`（或允许 `tcas` 命令脱离沙箱执行）。

### 3. 在项目中启用

进入项目根目录打开 workspace（`tcasd` 会在首次调用时自动在后台拉起）：

```sh
cd ~/Code/your-project
tcas workspace open

# 如果项目还有并排的关联仓库，可一并加入（或使用 harness 的 --add-dir / /add-dir 自动并入）
tcas workspace add ../your-backend ../your-ui-kit
```

之后在该目录启动多个 Claude Code 或 AGY 会话即可并行工作。

## 常用命令

| 命令 | 用途 |
|---|---|
| `tcas workspace open [目录…]` | 开启 workspace（自动发现仓库并挂载配置式 Git 钩子） |
| `tcas workspace add <目录…>` | 将并排仓库或外部 worktree 加入当前 workspace |
| `tcas workspace set mode <auto\|strict>` | 切换默认自动租约（`auto`）或强制先申请（`strict`） |
| `tcas workspace close` | 关闭当前 workspace 并移除挂载的 Git 钩子 |
| `tcas status` | 查看当前 workspace 的活跃会话、持有租约、停滞/遗留改动 |
| `tcas lease acquire <路径…> [--brief "…"]` | 事先按目录或文件申请租约（全有或全无） |
| `tcas lease wait <路径…> [--brief "…"]` | 后台等待路径释放并自动接手租约 |
| `tcas lease release [<路径…>] [--all]` | 释放持有的租约（`--force` 供用户强制释放） |
| `tcas lease who <路径…>` | 查询路径当前由哪个会话持有，或因何规则豁免/排除 |
| `tcas commit -m "<说明>" [<路径…>]` | 只提交本会话改过的文件并释放对应文件租约 |
| `tcas log [-n N] [--follow]` | 查看或实时追踪 workspace 事件流 |
| `tcas stats [--since 7d]` | 统计争用热点、等待耗时、事先申请率与护栏拦截情况 |
| `tcas doctor` | 检查 `tcasd`、Git 钩子支持、各 Harness 插件与豁免目录状态 |

## 架构与文档

系统仅由两个本机原生程序组成，通过 `~/.tcas/tcasd.sock`（Unix Domain Socket）通信，状态持久化在 `~/.tcas/tcasd.db`（SQLite WAL）：

- **`tcas`**：用户 CLI、Agent 租约与提交工具，以及各家 Harness 共用的超轻量 Hook 适配层（`tcas hook claude`、`tcas hook agy`）与 Git 钩子入口（`tcas git-hook`）。
- **`tcasd`**：每台机器一个的后台守护进程，负责 workspace 拓扑、会话存活检测、租约引擎、等待唤醒、Bash 快照对账与事件统计。

详细规格说明见 [`docs/v1/`](docs/v1/README.md)：

- [概览与设计取舍](docs/v1/README.md)
- [租约模型与不变量](docs/v1/leases.md)
- [Harness 插件、Hook 与实测记录](docs/v1/harness.md)
- [Daemon、存储、CLI 与 `.tcas.toml` 配置](docs/v1/daemon-and-cli.md)
- [实施计划与验收用例](docs/v1/plan.md)
- [多会话并行完整演练](docs/v1/walkthrough.md)
